use crate::{PixelFormat, Rect, VncError, VncEvent};
use anyhow::Result;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    sync::mpsc::Sender,
};
use tracing::error;

use super::{uninit_vec, zlib::ZlibReader};

fn read_run_length(reader: &mut ZlibReader) -> Result<usize> {
    let mut run_length_part;
    let mut run_length = 1;
    loop {
        run_length_part = reader.read_u8()?;
        run_length += run_length_part as usize;
        if 255 != run_length_part {
            break;
        }
    }
    Ok(run_length)
}

fn copy_true_color(
    reader: &mut ZlibReader,
    pixels: &mut Vec<u8>,
    pad: bool,
    compressed_bpp: usize,
    bpp: usize,
) -> Result<()> {
    let mut buf = [255; 4];
    std::io::Read::read_exact(
        reader,
        &mut buf[pad as usize..pad as usize + compressed_bpp],
    )?;
    pixels.extend_from_slice(&buf[..bpp]);
    Ok(())
}

fn copy_indexed(palette: &[u8], pixels: &mut Vec<u8>, bpp: usize, index: u8) {
    let start = index as usize * bpp;
    pixels.extend_from_slice(&palette[start..start + bpp])
}

pub struct Decoder {
    decompressor: Option<flate2::Decompress>,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            decompressor: Some(flate2::Decompress::new(true)),
        }
    }

    pub async fn decode<S>(
        &mut self,
        format: &PixelFormat,
        rect: &Rect,
        input: &mut S,
        output: &Sender<VncEvent>,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let data_len = input.read_u32().await? as usize;
        let mut zlib_data = uninit_vec(data_len);
        input.read_exact(&mut zlib_data).await?;
        let decompressor = self.decompressor.take().unwrap();
        let mut reader = ZlibReader::new(decompressor, &zlib_data);

        let bpp = format.bits_per_pixel as usize / 8;
        let pixel_mask = (format.red_max as u32) << format.red_shift
            | (format.green_max as u32) << format.green_shift
            | (format.blue_max as u32) << format.blue_shift;

        let (compressed_bpp, alpha_at_first) =
            if format.bits_per_pixel == 32 && format.true_color_flag > 0 && format.depth <= 24 {
                if pixel_mask & 0x000000ff == 0 {
                    // rgb at the most significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data is excepted to be [rgb.0, rgb.1, rgb.2, alpha]
                    // otherwise the decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    (3, format.big_endian_flag == 0)
                } else if pixel_mask & 0xff000000 == 0 {
                    // rgb at the least significant bits
                    // if format.big_endian_flag is set
                    // then decompressed data should be [alpha, rgb.0, rgb.1, rgb.2]
                    // otherwise the decompressed data should be [rgb.0, rgb.1, rgb.2, alpha]
                    (3, format.big_endian_flag > 0)
                } else {
                    (4, false)
                }
            } else {
                (bpp, false)
            };
        let mut palette = Vec::with_capacity(128 * bpp);

        let mut y = 0;
        while y < rect.height {
            let height = if y + 64 > rect.height {
                rect.height - y
            } else {
                64
            };
            let mut x = 0;
            while x < rect.width {
                let width = if x + 64 > rect.width {
                    rect.width - x
                } else {
                    64
                };
                let pixel_count = height as usize * width as usize;

                let control = reader.read_u8()?;
                let is_rle = control & 0x80 > 0;
                let palette_size = control & 0x7f;
                palette.truncate(0);

                for _ in 0..palette_size {
                    copy_true_color(
                        &mut reader,
                        &mut palette,
                        alpha_at_first,
                        compressed_bpp,
                        bpp,
                    )?
                }

                let mut pixels = Vec::with_capacity(pixel_count * bpp);
                match (is_rle, palette_size) {
                    (false, 0) => {
                        // True Color pixels
                        for _ in 0..pixel_count {
                            copy_true_color(
                                &mut reader,
                                &mut pixels,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            )?
                        }
                    }
                    (false, 1) => {
                        // Color fill
                        for _ in 0..pixel_count {
                            copy_indexed(&palette, &mut pixels, bpp, 0)
                        }
                    }
                    (false, 2..=16) => {
                        // Indexed pixels
                        let bits_per_index = match palette_size {
                            2 => 1,
                            3..=4 => 2,
                            5..=16 => 4,
                            _ => unreachable!(),
                        };
                        for _ in 0..height {
                            for _ in 0..width {
                                let index = reader.read_bits(bits_per_index)?;
                                copy_indexed(&palette, &mut pixels, bpp, index)
                            }
                            reader.align();
                        }
                    }
                    (true, 0) => {
                        // True Color RLE
                        let mut count = 0;
                        let mut pixel = Vec::new();
                        while count < pixel_count {
                            pixel.truncate(0);
                            copy_true_color(
                                &mut reader,
                                &mut pixel,
                                alpha_at_first,
                                compressed_bpp,
                                bpp,
                            )?;
                            let run_length = read_run_length(&mut reader)?;
                            for _ in 0..run_length {
                                pixels.extend(&pixel)
                            }
                            count += run_length;
                        }
                    }
                    (true, 2..=127) => {
                        // Indexed RLE
                        let mut count = 0;
                        while count < pixel_count {
                            let control = reader.read_u8()?;
                            let longer_than_one = control & 0x80 > 0;
                            let index = control & 0x7f;
                            let run_length = if longer_than_one {
                                read_run_length(&mut reader)?
                            } else {
                                1
                            };
                            for _ in 0..run_length {
                                copy_indexed(&palette, &mut pixels, bpp, index);
                            }
                            count += run_length;
                        }
                    }
                    _ => {
                        error!("ZRLE subencoding error");
                        return Err(VncError::InvalidImageData.into());
                    }
                }
                output
                    .send(VncEvent::RawImage(
                        Rect {
                            x: rect.x + x,
                            y: rect.y + y,
                            width,
                            height,
                        },
                        pixels,
                    ))
                    .await?;
                x += width;
            }
            y += height;
        }

        self.decompressor = Some(reader.into_inner()?);

        Ok(())
    }
}
