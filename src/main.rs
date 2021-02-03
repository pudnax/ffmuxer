use encoder::Encoder;
use ffmpeg::{
    codec::{self, packet::Packet},
    encoder, format, packet, sys,
    util::{self, error, frame, rational::Rational},
    Dictionary,
};
use ffmpeg_next as ffmpeg;
use std::{fs::File, io::Write, path::Path, time::Instant};

// const DEFAULT_X264_OPTS: &str = "preset=medium";
const DEFAULT_X264_OPTS: &str = "preset=veryslow,crf=18";

#[derive(Debug, Clone, Copy)]
struct VideoParams {
    fps: i32,
    width: u32,
    height: u32,
    bitrate: usize,
}

struct Recorder {
    encoder: encoder::Video,
    logging_enabled: bool,
    frame_count: usize,
    last_log_frame_count: usize,
    starting_time: Instant,
    last_log_time: Instant,
}

impl Recorder {
    fn new(
        params: &VideoParams,
        x264_opts: Dictionary,
        enable_logging: bool,
    ) -> Result<Self, error::Error> {
        let codec = encoder::find(codec::Id::MPEG2VIDEO).unwrap();

        let context = codec::Context::new();
        let mut encoder = context.encoder().video()?;

        encoder.set_bit_rate(params.bitrate);
        encoder.set_width(params.width);
        encoder.set_height(params.height);
        encoder.set_aspect_ratio((params.height as i32, params.width as i32));
        encoder.set_time_base((1, params.fps));
        encoder.set_gop(10); // 12
        encoder.set_frame_rate(Some((params.fps, 1)));
        encoder.set_max_b_frames(2);
        // codec.set_mb_decision(encoder::Decision::RateDistortion); //  MPEG1Video
        encoder.set_format(format::Pixel::YUV420P);

        let encoder = encoder.open_as_with(codec, x264_opts)?;
        Ok(Self {
            encoder,
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
        packet: &mut packet::Packet,
        f: &mut std::fs::File,
    ) -> Result<(), util::error::Error> {
        self.encoder.send_frame(frame)?;

        loop {
            let timestamp = frame.timestamp();
            self.log_progress(f64::from(Rational(timestamp.unwrap_or(0) as i32, 1)));

            match self.encoder.receive_packet(packet) {
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

    fn log_progress(&mut self, timestamp: f64) {
        if !self.logging_enabled
            || (self.frame_count - self.last_log_frame_count < 100
                && self.last_log_time.elapsed().as_secs_f64() < 1.0)
        {
            return;
        }
        println!(
            "time elpased: \t{:8.2}\tframe count: {:8}\ttimestamp: {:8.2}",
            self.starting_time.elapsed().as_secs_f64(),
            self.frame_count,
            timestamp
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

    let x264_opts = parse_opts(DEFAULT_X264_OPTS.to_string()).unwrap();
    let video_params = VideoParams {
        fps: 25,
        width: 352,
        height: 288,
        bitrate: 400_000,
    };
    let mut recorder = Recorder::new(&video_params, x264_opts, true)?;

    let mut packet = Packet::empty();

    let filename = Path::new("out.mp4");

    let mut f = File::create(filename)?;
    let mut frame = alloc_picture(
        recorder.encoder.format(),
        recorder.encoder.width(),
        recorder.encoder.height(),
    );
    let linesize = unsafe { (*frame.as_ptr()).linesize };

    for i in 0..25 {
        println!("Frame: {}", i);

        for y in 0..frame.height() as usize {
            for x in 0..frame.width() as usize {
                frame.data_mut(0)[y * linesize[0] as usize] = ((128 + 7) as i64 + i * 2) as u8;
                frame.data_mut(1)[(y >> 1) * linesize[1] as usize + (x >> 1)] =
                    ((64 + x) as i64 + i * 5) as u8;
                frame.data_mut(2)[(y >> 1) * linesize[2] as usize + (x >> 1)] =
                    ((64 + x) as i64 + i * 5) as u8;
            }
        }

        frame.set_pts(Some(i as i64));

        recorder.encode(&frame, &mut packet, &mut f)?;
    }

    recorder.encoder.send_eof()?;

    f.write_all(&[0, 0, 1, 0xb7]).unwrap();

    f.sync_all().unwrap();

    Ok(())
}

fn alloc_picture(format: format::Pixel, width: u32, height: u32) -> frame::video::Video {
    let mut frame = frame::video::Video::empty();
    unsafe {
        frame.alloc(format, width, height);
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
