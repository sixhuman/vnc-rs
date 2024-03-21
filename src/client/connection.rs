use futures::TryStreamExt;
use tokio_stream::wrappers::ReceiverStream;

use std::{future::Future, sync::Arc, vec};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        mpsc::{
            channel,
            error::{TryRecvError, TrySendError},
            Receiver, Sender,
        },
        oneshot, Mutex,
    },
};
use tokio_util::compat::*;
use tracing::*;

use crate::{codec, PixelFormat, Rect, VncEncoding, VncError, VncEvent, X11Event};
const CHANNEL_SIZE: usize = 4096;

#[cfg(not(target_arch = "wasm32"))]
use tokio::spawn;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local as spawn;

use super::messages::{ClientMsg, ServerMsg};

struct ImageRect {
    rect: Rect,
    encoding: VncEncoding,
}

impl From<[u8; 12]> for ImageRect {
    fn from(buf: [u8; 12]) -> Self {
        Self {
            rect: Rect {
                x: (buf[0] as u16) << 8 | buf[1] as u16,
                y: (buf[2] as u16) << 8 | buf[3] as u16,
                width: (buf[4] as u16) << 8 | buf[5] as u16,
                height: (buf[6] as u16) << 8 | buf[7] as u16,
            },
            encoding: ((buf[8] as u32) << 24
                | (buf[9] as u32) << 16
                | (buf[10] as u32) << 8
                | (buf[11] as u32))
                .into(),
        }
    }
}

impl ImageRect {
    async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut rect_buf = [0_u8; 12];
        reader.read_exact(&mut rect_buf).await?;
        Ok(rect_buf.into())
    }
}

pub struct VncInner {
    name: String,
    screen: (u16, u16),
    input_ch: Arc<Mutex<Sender<ClientMsg>>>,
    output_ch: Arc<Mutex<Receiver<VncEvent>>>,
    decoding_stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    net_conn_stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    closed: bool,
}

/// The instance of a connected vnc client

impl VncInner {
    pub async fn new<S>(
        mut stream: S,
        shared: bool,
        mut pixel_format: Option<PixelFormat>,
        encodings: Vec<VncEncoding>,
    ) -> Result<Self, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (conn_ch_tx, conn_ch_rx) = channel(CHANNEL_SIZE);
        let (input_ch_tx, input_ch_rx) = channel(CHANNEL_SIZE);
        let (output_ch_tx, output_ch_rx) = channel(CHANNEL_SIZE);
        let (decoding_stop_tx, decoding_stop_rx) = oneshot::channel();
        let (net_conn_stop_tx, net_conn_stop_rx) = oneshot::channel();

        trace!("client init msg");
        send_client_init(&mut stream, shared).await?;

        trace!("server init msg");
        let (name, (width, height)) =
            read_server_init(&mut stream, &mut pixel_format, &|e| async {
                output_ch_tx.send(e).await?;
                Ok(())
            })
            .await?;

        trace!("client encodings: {:?}", encodings);
        send_client_encoding(&mut stream, encodings).await?;

        trace!("Require the first frame");
        input_ch_tx
            .send(ClientMsg::FramebufferUpdateRequest(
                Rect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
                0,
            ))
            .await?;

        // start the decoding thread
        spawn(async move {
            trace!("Decoding thread starts");
            let mut conn_ch_rx = {
                let conn_ch_rx = ReceiverStream::new(conn_ch_rx).into_async_read();
                FuturesAsyncReadCompatExt::compat(conn_ch_rx)
            };

            let output_func = |e| async {
                output_ch_tx.send(e).await?;
                Ok(())
            };

            let pf = pixel_format.as_ref().unwrap();
            if let Err(e) =
                asycn_vnc_read_loop(&mut conn_ch_rx, pf, &output_func, decoding_stop_rx).await
            {
                if let VncError::IoError(e) = e {
                    if let std::io::ErrorKind::UnexpectedEof = e.kind() {
                        // this should be a normal case when the network connection disconnects
                        // and we just send an EOF over the inner bridge between the process thread and the decode thread
                        // do nothing here
                    } else {
                        error!("Error occurs during the decoding {:?}", e);
                        let _ = output_func(VncEvent::Error(e.to_string())).await;
                    }
                } else {
                    error!("Error occurs during the decoding {:?}", e);
                    let _ = output_func(VncEvent::Error(e.to_string())).await;
                }
            }
            trace!("Decoding thread stops");
        });

        // start the traffic process thread
        spawn(async move {
            trace!("Net Connection thread starts");
            let _ =
                async_connection_process_loop(stream, input_ch_rx, conn_ch_tx, net_conn_stop_rx)
                    .await;
            trace!("Net Connection thread stops");
        });

        info!("VNC Client {name} starts");
        Ok(Self {
            name,
            screen: (width, height),
            input_ch: Arc::new(Mutex::new(input_ch_tx)),
            output_ch: Arc::new(Mutex::new(output_ch_rx)),
            decoding_stop: Arc::new(Mutex::new(Some(decoding_stop_tx))),
            net_conn_stop: Arc::new(Mutex::new(Some(net_conn_stop_tx))),
            closed: false,
        })
    }

    pub async fn input(&self, event: X11Event) -> Result<(), VncError> {
        if self.closed {
            Err(VncError::ClientNotRunning)
        } else {
            let msg = match event {
                X11Event::Refresh => ClientMsg::FramebufferUpdateRequest(
                    Rect {
                        x: 0,
                        y: 0,
                        width: self.screen.0,
                        height: self.screen.1,
                    },
                    1,
                ),
                X11Event::KeyEvent(key) => ClientMsg::KeyEvent(key.keycode, key.down),
                X11Event::PointerEvent(mouse) => {
                    ClientMsg::PointerEvent(mouse.position_x, mouse.position_y, mouse.bottons)
                }
                X11Event::CopyText(text) => ClientMsg::ClientCutText(text),
            };
            self.input_ch.lock().await.send(msg).await?;
            Ok(())
        }
    }

    pub async fn recv_event(&self) -> Result<VncEvent, VncError> {
        if self.closed {
            Err(VncError::ClientNotRunning)
        } else {
            match self.output_ch.lock().await.recv().await {
                Some(e) => Ok(e),
                None => {
                    // self.closed = true;
                    Err(VncError::ClientNotRunning)
                }
            }
        }
    }

    pub async fn poll_event(&self) -> Result<Option<VncEvent>, VncError> {
        if self.closed {
            Err(VncError::ClientNotRunning)
        } else {
            match self.output_ch.lock().await.try_recv() {
                Err(TryRecvError::Disconnected) => {
                    // self.closed = true;
                    Err(VncError::ClientNotRunning)
                }
                Err(TryRecvError::Empty) => Ok(None),
                Ok(e) => Ok(Some(e)),
            }
            // Ok(self.output_ch.recv().await)
        }
    }

    /// Stop the VNC engine and release resources
    ///
    pub async fn close(&mut self) -> Result<(), VncError> {
        if self.net_conn_stop.lock().await.is_some() {
            let net_conn_stop: oneshot::Sender<()> =
                self.net_conn_stop.lock().await.take().unwrap();
            let _ = net_conn_stop.send(());
        }
        if self.decoding_stop.lock().await.is_some() {
            let decoding_stop = self.decoding_stop.lock().await.take().unwrap();
            let _ = decoding_stop.send(());
        }
        self.closed = true;
        Ok(())
    }
}

impl Drop for VncInner {
    fn drop(&mut self) {
        info!("VNC Client {} stops", self.name);
        let _ = self.close();
    }
}

impl Clone for VncInner {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            screen: self.screen.clone(),
            input_ch: self.input_ch.clone(),
            output_ch: self.output_ch.clone(),
            decoding_stop: self.decoding_stop.clone(),
            net_conn_stop: self.net_conn_stop.clone(),
            closed: self.closed.clone(),
        }
    }
}

// pub struct VncClient {
//     inner: Arc<Mutex<VncInner>>,
// }

// impl VncClient {
//     pub(super) async fn new<S>(
//         stream: S,
//         shared: bool,
//         pixel_format: Option<PixelFormat>,
//         encodings: Vec<VncEncoding>,
//     ) -> Result<Self, VncError>
//     where
//         S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
//     {
//         Ok(Self {
//             inner: Arc::new(Mutex::new(
//                 VncInner::new(stream, shared, pixel_format, encodings).await?,
//             )),
//         })
//     }

//     /// Input a `X11Event` from the frontend
//     ///
//     pub async fn input(&self, event: X11Event) -> Result<(), VncError> {
//         self.inner.lock().await.input(event).await
//     }

//     /// Receive a `VncEvent` from the engine
//     /// This function will block until a `VncEvent` is received
//     ///
//     pub async fn recv_event(&self) -> Result<VncEvent, VncError> {
//         self.inner.lock().await.recv_event().await
//     }

//     /// polling `VncEvent` from the engine and give it to the client
//     ///
//     pub async fn poll_event(&self) -> Result<Option<VncEvent>, VncError> {
//         self.inner.lock().await.poll_event().await
//     }

//     /// Stop the VNC engine and release resources
//     ///
//     pub async fn close(&self) -> Result<(), VncError> {
//         self.inner.lock().await.close()
//     }
// }

// impl Clone for VncClient {
//     fn clone(&self) -> Self {
//         Self {
//             inner: self.inner.clone(),
//         }
//     }
// }

async fn send_client_init<S>(stream: &mut S, shared: bool) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    trace!("Send shared flag: {}", shared);
    stream.write_u8(shared as u8).await?;
    Ok(())
}

async fn read_server_init<S, F, Fut>(
    stream: &mut S,
    pf: &mut Option<PixelFormat>,
    output_func: &F,
) -> Result<(String, (u16, u16)), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    // +--------------+--------------+------------------------------+
    // | No. of bytes | Type [Value] | Description                  |
    // +--------------+--------------+------------------------------+
    // | 2            | U16          | framebuffer-width in pixels  |
    // | 2            | U16          | framebuffer-height in pixels |
    // | 16           | PIXEL_FORMAT | server-pixel-format          |
    // | 4            | U32          | name-length                  |
    // | name-length  | U8 array     | name-string                  |
    // +--------------+--------------+------------------------------+

    let screen_width = stream.read_u16().await?;
    let screen_height = stream.read_u16().await?;
    let mut send_our_pf = false;

    output_func(VncEvent::SetResolution(
        (screen_width, screen_height).into(),
    ))
    .await?;

    let pixel_format = PixelFormat::read(stream).await?;
    if pf.is_none() {
        output_func(VncEvent::SetPixelFormat(pixel_format)).await?;
        let _ = pf.insert(pixel_format);
    } else {
        send_our_pf = true;
    }

    let name_len = stream.read_u32().await?;
    let mut name_buf = vec![0_u8; name_len as usize];
    stream.read_exact(&mut name_buf).await?;
    let name = String::from_utf8_lossy(&name_buf).into_owned();

    if send_our_pf {
        trace!("Send customized pixel format {:#?}", pf);
        ClientMsg::SetPixelFormat(*pf.as_ref().unwrap())
            .write(stream)
            .await?;
    }
    Ok((name, (screen_width, screen_height)))
}

async fn send_client_encoding<S>(
    stream: &mut S,
    encodings: Vec<VncEncoding>,
) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    ClientMsg::SetEncodings(encodings).write(stream).await?;
    Ok(())
}

async fn asycn_vnc_read_loop<S, F, Fut>(
    stream: &mut S,
    pf: &PixelFormat,
    output_func: &F,
    mut stop_ch: oneshot::Receiver<()>,
) -> Result<(), VncError>
where
    S: AsyncRead + Unpin,
    F: Fn(VncEvent) -> Fut,
    Fut: Future<Output = Result<(), VncError>>,
{
    let mut raw_decoder = codec::RawDecoder::new();
    let mut zrle_decoder = codec::ZrleDecoder::new();
    let mut tight_decoder = codec::TightDecoder::new();
    let mut trle_decoder = codec::TrleDecoder::new();
    let mut cursor = codec::CursorDecoder::new();

    // main decoding loop
    while let Err(oneshot::error::TryRecvError::Empty) = stop_ch.try_recv() {
        let server_msg = ServerMsg::read(stream).await?;
        trace!("Server message got: {:?}", server_msg);
        match server_msg {
            ServerMsg::FramebufferUpdate(rect_num) => {
                for _ in 0..rect_num {
                    let rect = ImageRect::read(stream).await?;
                    // trace!("Encoding: {:?}", rect.encoding);

                    match rect.encoding {
                        VncEncoding::Raw => {
                            // println!("received raw output");
                            raw_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::CopyRect => {
                            // println!("received copy rect output");
                            let source_x = stream.read_u16().await?;
                            let source_y = stream.read_u16().await?;
                            let mut src_rect = rect.rect;
                            src_rect.x = source_x;
                            src_rect.y = source_y;
                            output_func(VncEvent::Copy(rect.rect, src_rect)).await?;
                        }
                        VncEncoding::Tight => {
                            // println!("received tight output");
                            tight_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::Trle => {
                            // println!("received trle output");
                            trle_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::Zrle => {
                            // println!("received zrle output");
                            zrle_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                        VncEncoding::CursorPseudo => {
                            // println!("received cursor output");
                            cursor.decode(pf, &rect.rect, stream, output_func).await?;
                        }
                        VncEncoding::DesktopSizePseudo => {
                            // println!("received desktop size output");
                            output_func(VncEvent::SetResolution(
                                (rect.rect.width, rect.rect.height).into(),
                            ))
                            .await?;
                        }
                        VncEncoding::LastRectPseudo => {
                            // println!("received last rect output");
                            break;
                        }
                        VncEncoding::JPEGQualityLevel0
                        | VncEncoding::JPEGQualityLevel6
                        | VncEncoding::JPEGQualityLevel4
                        | VncEncoding::JPEGQualityLevel9 => {
                            // println!("received jpeg quality level 0 output");
                            tight_decoder
                                .decode(pf, &rect.rect, stream, output_func)
                                .await?;
                        }
                    }
                }
            }
            // SetColorMapEntries,
            ServerMsg::Bell => {
                output_func(VncEvent::Bell).await?;
            }
            ServerMsg::ServerCutText(text) => {
                output_func(VncEvent::Text(text)).await?;
            }
        }
    }
    Ok(())
}

async fn async_connection_process_loop<S>(
    stream: S,
    mut input_ch: Receiver<ClientMsg>,
    conn_ch: Sender<std::io::Result<Vec<u8>>>,
    mut stop_ch: oneshot::Receiver<()>,
) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut rh, mut wh) = tokio::io::split(stream);

    let jh = tokio::spawn(async move {
        let mut buffer = [0; 65535];
        let mut pending = 0;

        loop {
            if pending > 0 {
                match conn_ch.try_send(Ok(buffer[0..pending].to_owned())) {
                    Err(TrySendError::Full(_message)) => (),
                    Err(TrySendError::Closed(_message)) => break,
                    Ok(()) => pending = 0,
                }
            }

            if pending == 0 {
                match rh.read(&mut buffer).await {
                    Ok(nread) => {
                        println!("read {} bytes", nread);
                        if nread > 0 {
                            match conn_ch.try_send(Ok(buffer[0..nread].to_owned())) {
                                Err(TrySendError::Full(_message)) => pending = nread,
                                Err(TrySendError::Closed(_message)) => break,
                                Ok(()) => (),
                            }
                        } else {
                            // According to the tokio's Doc
                            // https://docs.rs/tokio/latest/tokio/io/trait.AsyncRead.html
                            // if nread == 0, then EOF is reached
                            trace!("Net Connection EOF detected");
                            break;
                        }
                    }
                    Err(e) => {
                        error!("{}", e.to_string());
                        break;
                    }
                };
            }
        }
    });

    // main traffic loop
    while let Err(oneshot::error::TryRecvError::Empty) = stop_ch.try_recv() {
        tokio::select! {
            Some(msg) = input_ch.recv() => {
                msg.write(&mut wh).await?;
            }
        }
    }

    // notify the decoding thread
    // let _ = conn_ch
    //     .send(Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)))
    //     .await;
    jh.await.expect("Error with join handle");
    Ok(())
}
