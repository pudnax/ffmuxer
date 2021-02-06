use codec::Id;
use encoder::Encoder;
use ffmpeg::{
    codec::{self, packet::Packet},
    encoder, format, packet, sys,
    util::{self, error, frame, rational::Rational},
    Codec, Dictionary, DictionaryRef, StreamMut,
};
use ffmpeg_next as ffmpeg;
use format::Flags;
use std::{fs::File, io::Write, path::Path, time::Instant};

// const DEFAULT_X264_OPTS: &str = "preset=medium";
const DEFAULT_X264_OPTS: &str = "preset=veryslow,crf=18";
const STREAM_FORMAT: format::Pixel = format::Pixel::YUV420P;

fn fill_yuv_image(pict: &mut frame::video::Video, frame_index: i64, width: usize, height: usize) {
    let i = frame_index;
    let linesize = unsafe { (*pict.as_ptr()).linesize };

    for y in 0..height {
        for x in 0..width {
            pict.data_mut(0)[y * linesize[0] as usize + x] = ((x + y) as i64 + i * 3) as u8;
        }
    }

    for y in 0..height - 1 {
        for x in 0..width {
            // dbg!(y * linesize[1] as usize + x);
            pict.data_mut(1)[y * ((linesize[1] - 1) / 2) as usize + x] =
                ((128 + y) as i64 + i * 2) as u8;
            pict.data_mut(2)[y * ((linesize[1] - 1) / 2) as usize + x] =
                ((64 + x) as i64 + i * 5) as u8;
        }
    }
}

#[allow(clippy::many_single_char_names)]
fn copy_image_as_yuv(image: &[u8], frame: &mut frame::Video, (width, _h): (usize, usize)) {
    let linesize = unsafe { (*frame.as_ptr()).linesize };
    for (index, chunk) in image.chunks_exact(4).enumerate() {
        let row = index % width;
        let col = index / width;
        let r = chunk[0] as f32;
        let g = chunk[1] as f32;
        let b = chunk[2] as f32;

        let y = (0.257 * r) + (0.504 * g) + (0.098 * b) + 16.;
        let u = -(0.148 * r) - (0.291 * g) + (0.439 * b) + 128.;
        let v = (0.439 * r) - (0.368 * g) - (0.071 * b) + 128.;

        frame.data_mut(0)[(row * linesize[0] as usize + col)] = y as u8;
        frame.data_mut(1)[(row >> 1) * linesize[1] as usize + (col >> 1)] = u as u8;
        frame.data_mut(2)[(row >> 1) * linesize[2] as usize + (col >> 1)] = v as u8;
    }
}

#[derive(Debug, Clone, Copy)]
struct VideoParams {
    fps: i32,
    width: u32,
    height: u32,
    bitrate: usize,
}

struct Recorder {
    encoder: encoder::video::Video,
    stream_info: (usize, Rational),
    logging_enabled: bool,
    frame_count: usize,
    last_log_frame_count: usize,
    starting_time: Instant,
    last_log_time: Instant,
}

impl Recorder {
    fn new(
        output: &mut format::context::Output,
        flags: Flags,
        params: &VideoParams,
        codec: &Codec,
        enable_logging: bool,
    ) -> Result<Self, error::Error> {
        // let codec = encoder::find(codec::Id::MPEG2VIDEO).unwrap();

        // let context = codec::Context::new();
        let context =
            unsafe { codec::Context::wrap(sys::avcodec_alloc_context3(codec.as_ptr()), None) };
        let mut encoder = context.encoder().video()?;

        let id: sys::AVCodecID = codec.id().into();
        unsafe { (*encoder.as_mut_ptr()).codec_id = id };
        encoder.set_bit_rate(params.bitrate);
        encoder.set_width(params.width);
        encoder.set_height(params.height);
        encoder.set_aspect_ratio((params.height as i32, params.width as i32));
        encoder.set_gop(10); // 12
        encoder.set_frame_rate(Some((params.fps, 1)));
        if encoder.id() == Id::MPEG2VIDEO {
            encoder.set_max_b_frames(2);
        }
        if encoder.id() == Id::MPEG1VIDEO {
            encoder.set_mb_decision(encoder::Decision::RateDistortion);
        }
        encoder.set_format(STREAM_FORMAT);

        let mut stream = output.add_stream(encoder.codec())?;
        stream.set_parameters(&encoder);
        encoder.set_time_base(stream.time_base());
        if flags == format::Flags::GLOBAL_HEADER {
            unsafe {
                (*encoder.as_mut_ptr()).flags |= codec::Flags::GLOBAL_HEADER.bits() as i32;
            }
        }

        let stream_info = (stream.index(), stream.time_base());
        // let encoder = encoder.open_as_with(codec, x264_opts)?;
        // let encoder = encoder.video()?;
        Ok(Self {
            encoder,
            stream_info,
            logging_enabled: enable_logging,
            frame_count: 0,
            last_log_frame_count: 0,
            starting_time: Instant::now(),
            last_log_time: Instant::now(),
        })
    }

    fn encode(
        &mut self,
        frame: &util::frame::Frame,
        (stream_index, stream_timebase): (usize, Rational),
        output: &mut format::context::Output,
    ) -> Result<(), util::error::Error> {
        self.encoder.send_frame(frame)?;
        let src_timebase = unsafe { (*self.encoder.as_ptr()).time_base };

        loop {
            let mut packet = {
                let p_info = frame.packet();
                let mut packet = codec::packet::Packet::empty();
                packet.set_pts(Some(p_info.pts));
                packet.set_dts(Some(p_info.dts));
                packet.set_duration(p_info.duration);
                packet.set_position(p_info.position as isize);
                packet
            };

            self.frame_count += 1;
            self.log_progress();

            match self.encoder.receive_packet(&mut packet) {
                Ok(_) => {}
                Err(error::Error::Other {
                    errno: error::EAGAIN,
                })
                | Err(error::Error::Eof) => return Ok(()),
                Err(e) => panic!("Error with: {}", e),
            }

            packet.rescale_ts(src_timebase, stream_timebase);
            packet.set_stream(stream_index);

            packet.write_interleaved(output)?;
        }
    }

    fn log_progress(&mut self) {
        if !self.logging_enabled
            || (self.frame_count - self.last_log_frame_count < 10
                && self.last_log_time.elapsed().as_secs_f64() < 1.0)
        {
            return;
        }
        println!(
            "time elpased: \t{:8.2}\tframe count: {:8}",
            self.starting_time.elapsed().as_secs_f64(),
            self.frame_count,
        );
        self.last_log_frame_count = self.frame_count;
        self.last_log_time = Instant::now();
    }
}

fn parse_opts<'a>(s: String) -> Option<Dictionary<'a>> {
    let mut dict = Dictionary::new();
    for keyval in s.split_terminator(',') {
        let tokens: Vec<&str> = keyval.split('=').collect();
        match tokens[..] {
            [key, val] => dict.set(key, val),
            _ => return None,
        }
    }
    Some(dict)
}

fn test_ffmpeg() -> Result<(), Box<dyn std::error::Error>> {
    ffmpeg::init()?;

    let filename = Path::new("out.mp4");

    let mut output = format::output(&filename)?;
    let format = output.format();
    let codec = {
        let id: Id = unsafe { (*format.as_ptr()).video_codec }.into();
        encoder::find(id).unwrap()
    };

    let x264_opts = parse_opts(DEFAULT_X264_OPTS.to_string()).unwrap();
    let video_params = VideoParams {
        fps: 60,
        width: 352,
        height: 288,
        bitrate: 400_000,
    };
    let mut recorder = Recorder::new(&mut output, format.flags(), &video_params, &codec, true)?;

    let mut frame = alloc_picture(
        recorder.encoder.format(),
        recorder.encoder.width(),
        recorder.encoder.height(),
    );
    frame.set_metadata(x264_opts);

    format::context::output::dump(&output, 0, filename.to_str());

    output.write_header()?;

    for i in 0..175 {
        println!("Frame: {}", i);

        fill_yuv_image(
            &mut frame,
            recorder.frame_count as i64,
            recorder.encoder.width() as usize,
            recorder.encoder.height() as usize,
        );

        // for y in 0..frame.height() as usize {
        //     for x in 0..frame.width() as usize {
        //         frame.data_mut(0)[y * linesize[0] as usize] = ((128 + 7) as i64 + i * 2) as u8;
        //         frame.data_mut(1)[(y >> 1) * linesize[1] as usize + (x >> 1)] =
        //             ((64 + x) as i64 + i * 5) as u8;
        //         frame.data_mut(2)[(y >> 1) * linesize[2] as usize + (x >> 1)] =
        //             ((64 + x) as i64 + i * 5) as u8;
        //     }
        // }

        frame.set_pts(Some(i as i64));

        recorder.encode(&frame, recorder.stream_info, &mut output)?;
    }

    output.write_trailer()?;

    // recorder.encoder.send_eof()?;

    Ok(())
}

fn alloc_picture(format: format::Pixel, width: u32, height: u32) -> frame::video::Video {
    let mut frame = frame::video::Video::empty();
    unsafe {
        frame.set_format(format);
        frame.set_width(width);
        frame.set_height(height);

        sys::av_frame_get_buffer(frame.as_mut_ptr(), 0);
    }
    assert!(unsafe { sys::av_frame_make_writable(frame.as_mut_ptr()) } >= 0);

    frame
}

fn _encode(
    encoder: &mut Encoder,
    frame: &util::frame::Frame,
    packet: &mut packet::Packet,
    f: &mut std::fs::File,
) -> Result<(), util::error::Error> {
    encoder.send_frame(frame)?;

    loop {
        match encoder.receive_packet(packet) {
            Ok(_) => {}
            Err(error::Error::Other {
                errno: error::EAGAIN,
            })
            | Err(error::Error::Eof) => return Ok(()),
            Err(e) => panic!("Error with: {}", e),
        }
        println!("Write packet {:?}, {}", packet.pts(), packet.size());
        f.write_all(packet.data().unwrap()).unwrap();
    }
}

fn main() {
    println!("Hello, world!");
    test_ffmpeg().unwrap();
}
