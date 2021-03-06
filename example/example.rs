// Copyright 2015 The Servo Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![feature(collections, core, env, io, libc, os, path, rustc_private, std_misc)]

extern crate clock_ticks;
extern crate libc;
extern crate "rust-media" as media;
extern crate sdl2;

#[macro_use]
extern crate log;

use libc::c_long;
use media::audioformat::{ConvertAudioFormat, Float32Interleaved, Float32Planar};
use media::container::{AudioTrack, ContainerReader, Frame, Track, VideoTrack};
use media::pixelformat::{ConvertPixelFormat, PixelFormat, Rgb24};
use media::playback::Player;
use media::videodecoder::{DecodedVideoFrame, VideoDecoder};
use sdl2::audio::{AudioCallback, AudioDevice, AudioSpecDesired};
use sdl2::event::{self, Event, WindowEventId};
use sdl2::keycode::KeyCode;
use sdl2::pixels::PixelFormatEnum;
use sdl2::rect::Rect;
use sdl2::render::{ACCELERATED, PRESENTVSYNC, RenderDriverIndex, Renderer, RendererParent};
use sdl2::render::{Texture, TextureAccess};
use sdl2::video::{OPENGL, RESIZABLE, Window, WindowPos};
use sdl2::{INIT_AUDIO, INIT_VIDEO, init};
use std::cmp;
use std::env;
use std::mem;
use std::old_io::fs::File;
use std::old_io::timer;
use std::slice;
use std::time::duration::Duration;

struct ExampleMediaPlayer {
    /// A reference timestamp at which playback began.
    playback_start_ticks: i64,
    /// A reference time in nanoseconds at which playback began.
    playback_start_wallclock_time: u64,
}

impl ExampleMediaPlayer {
    fn new() -> ExampleMediaPlayer {
        ExampleMediaPlayer {
            playback_start_ticks: 0,
            playback_start_wallclock_time: clock_ticks::precise_time_ns(),
        }
    }

    fn resync(&mut self, ticks: i64) {
        self.playback_start_ticks = ticks;
        self.playback_start_wallclock_time = clock_ticks::precise_time_ns()
    }

    /// Polls events so we can quit if the user wanted to. Returns true to continue or false to
    /// quit.
    fn poll_events(&mut self, player: &mut Player) -> bool {
        loop {
            match event::poll_event() {
                Event::None => break,
                Event::Quit {
                    ..
                } | Event::KeyDown {
                    keycode: KeyCode::Escape,
                    ..
                } => {
                    return false
                }
                Event::Window {
                    win_event_id: WindowEventId::Resized,
                    ..
                } => {
                    if let Some(last_frame_time) = player.last_frame_presentation_time() {
                        self.resync(last_frame_time.ticks)
                    }
                }
                _ => {}
            }
        }

        true
    }
}

struct ExampleVideoRenderer<'a> {
    /// The SDL renderer.
    renderer: &'a Renderer,
    /// The YUV texture we're using.
    texture: Texture<'a>,
}

impl<'a> ExampleVideoRenderer<'a> {
    fn new<'b>(renderer: &'b Renderer, video_format: SdlVideoFormat, video_height: i32)
               -> ExampleVideoRenderer<'b> {
        ExampleVideoRenderer {
            renderer: renderer,
            texture: renderer.create_texture(video_format.sdl_pixel_format,
                                             TextureAccess::Streaming,
                                             (video_format.sdl_width as i32,
                                              video_height)).unwrap(),
        }
    }

    fn present(&mut self, image: Box<DecodedVideoFrame + 'static>, player: &mut Player) {
        let video_track_number = player.video_track_number().unwrap();
        let reader = &mut *player.reader;
        let video_track = reader.track_by_number(video_track_number as c_long);
        let video_track = video_track.as_video_track().unwrap();

        let rect = if let &RendererParent::Window(ref window) = self.renderer.get_parent() {
            let (width, height) = window.get_size();
            Rect::new(0, 0, width, height)
        } else {
            panic!("Renderer parent wasn't a window!")
        };

        self.upload(image, &*video_track);
        let mut drawer = self.renderer.drawer();
        drawer.copy(&self.texture, None, Some(rect));
        drawer.present();
    }

    fn upload(&mut self, image: Box<DecodedVideoFrame + 'static>, video_track: &VideoTrack) {
        drop(self.texture.with_lock(None, |pixels, stride| {
            // FIXME(pcwalton): Workaround for rust-sdl2#331: the pixels array may be too small.
            let output_video_format = SdlVideoFormat::from_video_track(video_track);
            let height = video_track.height() as usize;
            let real_length = match output_video_format.media_pixel_format {
                PixelFormat::I420 => {
                    stride as usize * height + 2 * ((stride / 2) as usize * (height / 2))
                }
                PixelFormat::Rgb24 => stride as usize * height,
                _ => {
                    panic!("SDL can't natively render in {:?}!",
                           output_video_format.media_pixel_format)
                }
            };
            let pixels = unsafe {
                mem::transmute::<&mut [u8],
                                 &mut [u8]>(slice::from_raw_mut_buf(&mut pixels.as_mut_ptr(),
                                                                    real_length))
            };
            upload_image(video_track, &*image, pixels, stride as i32)
        }));
    }
}

/// SDL cannot natively display all pixel formats that `rust-media` supports. Therefore we may have
/// to do pixel format conversion ourselves. This structure contains the mapping from the pixel
/// format of the codec to the nearest matching SDL format.
///
/// Additionally, SDL is buggy with odd (as in, the opposite of even) video widths in some drivers.
/// So we have to store an "SDL width" for each video, which may be different from the real video
/// width. See:
///
///     https://trac.ffmpeg.org/attachment/ticket/1322/0001-ffplay-fix-odd-YUV-width-by-cropping-
///     the-video.patch
///
struct SdlVideoFormat {
    media_pixel_format: PixelFormat<'static>,
    sdl_pixel_format: PixelFormatEnum,
    sdl_width: u16,
}

impl SdlVideoFormat {
    fn from_video_track(video_track: &VideoTrack) -> SdlVideoFormat {
        let (media_pixel_format, sdl_pixel_format) = match video_track.pixel_format() {
            PixelFormat::I420 | PixelFormat::NV12 => (PixelFormat::I420, PixelFormatEnum::IYUV),
            PixelFormat::Indexed(_) | PixelFormat::Rgb24 => {
                (PixelFormat::Rgb24, PixelFormatEnum::RGB24)
            }
        };
        SdlVideoFormat {
            media_pixel_format: media_pixel_format,
            sdl_pixel_format: sdl_pixel_format,
            sdl_width: video_track.width() & !1,
        }
    }
}

pub struct ExampleAudioRenderer {
    samples: Vec<f32>,
}

impl AudioCallback<f32> for ExampleAudioRenderer {
    fn callback(&mut self, out: &mut [f32]) {
        if self.samples.len() < out.len() {
            // Zero out the buffer to avoid damaging the listener's eardrums.
            warn!("audio underrun");
            for value in out.iter_mut() {
                *value = 0.0
            }
        }

        let mut leftovers = Vec::new();
        for (i, sample) in mem::replace(&mut self.samples, Vec::new()).into_iter().enumerate() {
            if i < out.len() {
                out[i] = sample
            } else {
                leftovers.push(sample);
            }
        }
        self.samples = leftovers
    }
}

impl ExampleAudioRenderer {
    pub fn new(sample_rate: f64, channels: u16) -> AudioDevice<ExampleAudioRenderer> {
        let desired_spec = AudioSpecDesired {
            freq: sample_rate as i32,
            channels: cmp::min(channels, 2) as u8,
            samples: 0,
            callback: ExampleAudioRenderer {
                samples: Vec::new(),
            },
        };
        desired_spec.open_audio_device(None, false).unwrap()
    }
}

fn enqueue_audio_samples(device: &mut AudioDevice<ExampleAudioRenderer>,
                         input_samples: &[Vec<f32>]) {
    // Gather up all the channels so we can perform audio format conversion.
    let channels = device.get_spec().channels;
    let input_samples: Vec<_> = input_samples.iter()
                                             .take(2)
                                             .map(|samples| samples.as_slice())
                                             .collect();

    // Make room for the samples in the output buffer.
    let output_channels = cmp::min(channels, 2);
    let mut output = device.lock();
    let output_index = output.samples.len();
    let input_sample_count = input_samples[0].len();
    let output_length = input_sample_count * output_channels as usize;
    output.samples.resize(output_index + output_length, 0.0);

    // Perform audio format conversion.
    Float32Planar.convert(&Float32Interleaved,
                          &mut [&mut output.samples[output_index..]],
                          input_samples.as_slice(),
                          output_channels as usize).unwrap();
}

fn upload_image(video_track: &VideoTrack,
                image: &DecodedVideoFrame,
                output_pixels: &mut [u8],
                output_stride: i32) {
    let height = video_track.height();
    let pixel_format = image.pixel_format();

    // Gather up all the input pixels and strides so we can do pixel format conversion.
    let lock = image.lock();
    let (mut input_pixels, mut input_strides) = (Vec::new(), Vec::new());
    for plane in range(0, pixel_format.planes()) {
        input_pixels.push(lock.pixels(plane));
        input_strides.push(image.stride(plane) as usize);
    }

    // Gather up the output pixels and strides.
    let output_video_format = SdlVideoFormat::from_video_track(&*video_track);
    let (mut output_pixels, output_strides) = match output_video_format.media_pixel_format {
        PixelFormat::I420 => {
            let (output_luma, output_chroma) =
                output_pixels.split_at_mut(output_stride as usize * height as usize);
            let output_chroma_stride = output_stride as usize / 2;
            let (output_u, output_v) =
                output_chroma.split_at_mut(output_chroma_stride as usize * (height / 2) as usize);
            (vec![output_luma, output_u, output_v],
             vec![output_stride as usize, output_chroma_stride, output_chroma_stride])
        }
        PixelFormat::Rgb24 => (vec![output_pixels], vec![output_stride as usize]),
        _ => panic!("SDL can't natively render in {:?}!", output_video_format.media_pixel_format),
    };

    // Perform pixel format conversion.
    pixel_format.convert(&output_video_format.media_pixel_format,
                         output_pixels.as_mut_slice(),
                         output_strides.as_slice(),
                         input_pixels.as_slice(),
                         input_strides.as_slice(),
                         output_video_format.sdl_width as usize,
                         height as usize).unwrap();
}

fn main() {
    let args: Vec<String> = env::args().map(|arg| arg.into_string().unwrap()).collect();
    if args.len() < 3 {
        println!("usage: example path-to-video-or-audio-file mime-type");
        return
    }

    sdl2::init(INIT_VIDEO | INIT_AUDIO);
    let file = Box::new(File::open(&Path::new(args[1].as_slice())).unwrap());

    let mut player = Player::new(file, args[2].as_slice());
    let mut media_player = ExampleMediaPlayer::new();

    let renderer = player.video_track_number().map(|video_track_number| {
        let video_track = player.reader.track_by_number(video_track_number as c_long);
        let video_track = video_track.as_video_track().unwrap();
        let window = Window::new("rust-media example",
                                 WindowPos::PosCentered,
                                 WindowPos::PosCentered,
                                 video_track.width() as i32,
                                 video_track.height() as i32,
                                 OPENGL | RESIZABLE).unwrap();
        Renderer::from_window(window, RenderDriverIndex::Auto, ACCELERATED | PRESENTVSYNC).unwrap()
    });
    let mut video_renderer = player.video_track_number().map(|video_track_number| {
        let video_track = player.reader.track_by_number(video_track_number as c_long);
        let video_track = video_track.as_video_track().unwrap();
        let video_format = SdlVideoFormat::from_video_track(&*video_track);
        ExampleVideoRenderer::new(renderer.as_ref().unwrap(),
                                  video_format,
                                  video_track.height() as i32)
    });

    let mut audio_renderer = player.audio_track_number().map(|audio_track_number| {
        let audio_track = player.reader.track_by_number(audio_track_number as c_long);
        let audio_track = audio_track.as_audio_track().unwrap();
        let renderer = ExampleAudioRenderer::new(audio_track.sampling_rate(),
                                                 audio_track.channels());
        renderer.resume();
        renderer
    });

    loop {
        if player.decode_frame().is_err() {
            break
        }

        let target_time_since_playback_start = (player.next_frame_presentation_time().unwrap() -
                                                media_player.playback_start_ticks).duration();
        let target_time = Duration::nanoseconds(media_player.playback_start_wallclock_time as i64)
            + target_time_since_playback_start;
        timer::sleep(target_time - Duration::nanoseconds(clock_ticks::precise_time_ns() as i64));

        let frame = match player.advance() {
            Ok(frame) => frame,
            Err(_) => break,
        };

        if let Some(ref mut video_renderer) = video_renderer {
            video_renderer.present(frame.video_frame.unwrap(), &mut player);
        }
        if let Some(ref mut audio_renderer) = audio_renderer {
            enqueue_audio_samples(audio_renderer, frame.audio_samples.unwrap().as_slice());
        }

        if !media_player.poll_events(&mut player) {
            break
        }
    }
}

