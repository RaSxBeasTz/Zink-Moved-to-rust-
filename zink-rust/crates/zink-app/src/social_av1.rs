use std::{mem::MaybeUninit, ptr::NonNull};

use anyhow::{Context, ensure};
use rav1d::{
    include::dav1d::{
        data::Dav1dData,
        dav1d::{Dav1dContext, Dav1dSettings},
        headers::DAV1D_PIXEL_LAYOUT_I420,
        picture::Dav1dPicture,
    },
    src::lib::{
        dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings, dav1d_flush,
        dav1d_get_picture, dav1d_open, dav1d_picture_unref, dav1d_send_data,
    },
};

// dav1d reports the negated C errno for "no picture available".
const NEED_MORE_DATA: i32 = -11;

pub(super) struct Av1Decoder {
    context: Option<Dav1dContext>,
    width: usize,
    height: usize,
}

impl Av1Decoder {
    pub(super) fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        let mut settings = MaybeUninit::<Dav1dSettings>::uninit();
        unsafe { dav1d_default_settings(NonNull::from(&mut settings).cast()) };
        let mut settings = unsafe { settings.assume_init() };
        settings.n_threads = 4;
        settings.max_frame_delay = 1;
        settings.frame_size_limit = width * height;
        settings.all_layers = 0;
        let mut context = None;
        let result = unsafe {
            dav1d_open(
                Some(NonNull::from(&mut context)),
                Some(NonNull::from(&mut settings)),
            )
        };
        ensure!(
            result.0 == 0 && context.is_some(),
            "open Rust AV1 decoder ({})",
            result.0
        );
        Ok(Self {
            context,
            width: width as usize,
            height: height as usize,
        })
    }

    pub(super) fn restart(&mut self) {
        if let Some(context) = self.context {
            unsafe { dav1d_flush(context) };
        }
    }

    pub(super) fn decode(&mut self, frame: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
        if frame.is_empty() {
            return Ok(None);
        }
        let mut data = Dav1dData::default();
        let mut data_ptr = Some(NonNull::from(&mut data));
        let buffer = unsafe { dav1d_data_create(data_ptr, frame.len()) };
        ensure!(!buffer.is_null(), "allocate AV1 input");
        unsafe { std::ptr::copy_nonoverlapping(frame.as_ptr(), buffer, frame.len()) };
        let result = (|| {
            let mut picture = None;
            loop {
                let status = unsafe { dav1d_send_data(self.context, data_ptr) }.0;
                ensure!(
                    status == 0 || status == NEED_MORE_DATA,
                    "submit AV1 picture ({status})"
                );
                let next = self.read_picture()?;
                ensure!(
                    status == 0 || next.is_some(),
                    "AV1 decoder made no progress"
                );
                picture = next.or(picture);
                if status == 0 {
                    return Ok(picture);
                }
                data_ptr = Some(NonNull::from(&mut data));
            }
        })();
        unsafe { dav1d_data_unref(Some(NonNull::from(&mut data))) };
        result
    }

    fn read_picture(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let mut picture = Dav1dPicture::default();
        let status =
            unsafe { dav1d_get_picture(self.context, Some(NonNull::from(&mut picture))) }.0;
        if status == NEED_MORE_DATA {
            return Ok(None);
        }
        ensure!(status == 0, "decode AV1 picture ({status})");
        let pixels = self.to_rgba(&picture);
        unsafe { dav1d_picture_unref(Some(NonNull::from(&mut picture))) };
        pixels.map(Some)
    }

    fn to_rgba(&self, picture: &Dav1dPicture) -> anyhow::Result<Vec<u8>> {
        let width = self.width;
        let height = self.height;
        ensure!(
            picture.p.layout == DAV1D_PIXEL_LAYOUT_I420
                && picture.p.bpc == 8
                && picture.p.w as usize == width
                && picture.p.h as usize == height,
            "unexpected AV1 picture format or size"
        );
        let [y, u, v] = picture.data;
        let y = y.context("AV1 luma plane missing")?.cast::<u8>();
        let u = u.context("AV1 chroma plane missing")?.cast::<u8>();
        let v = v.context("AV1 chroma plane missing")?.cast::<u8>();
        let [y_pitch, uv_pitch] = picture.stride;
        ensure!(
            y_pitch >= width as isize && uv_pitch >= (width / 2) as isize,
            "invalid AV1 picture stride"
        );
        let mut rgba = vec![0; width * height * 4];
        for (row, pixels) in rgba.chunks_exact_mut(width * 4).enumerate() {
            // The decoder owns these planes until dav1d_picture_unref.
            let (y, u, v) = unsafe {
                let y =
                    std::slice::from_raw_parts(y.as_ptr().offset(row as isize * y_pitch), width);
                let chroma_row = (row / 2) as isize * uv_pitch;
                let u = std::slice::from_raw_parts(u.as_ptr().offset(chroma_row), width / 2);
                let v = std::slice::from_raw_parts(v.as_ptr().offset(chroma_row), width / 2);
                (y, u, v)
            };
            for (((pair, ys), &u), &v) in pixels
                .chunks_exact_mut(8)
                .zip(y.chunks_exact(2))
                .zip(u)
                .zip(v)
            {
                super::write_nv12_pair(pair, ys, super::nv12_chroma(&[u, v]));
            }
        }
        Ok(rgba)
    }
}

impl Drop for Av1Decoder {
    fn drop(&mut self) {
        unsafe { dav1d_close(Some(NonNull::from(&mut self.context))) };
    }
}
