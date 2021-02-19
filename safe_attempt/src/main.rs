use codec::Id;
use ffmpeg::{
    codec, encoder, format,
    packet::Mut,
    software, sys,
    util::{self, frame, rational::Rational},
    ChannelLayout, Dictionary, StreamMut,
};
use ffmpeg_next as ffmpeg;
use std::rc::Rc;
use std::{path::Path, time::Instant};

// const DEFAULT_X264_OPTS: &str = "";
// const DEFAULT_X264_OPTS: &str = "preset=medium";
const DEFAULT_X264_OPTS: &str = "preset=veryslow,crf=18,vpre=fast";
const STREAM_FORMAT: format::Pixel = format::Pixel::YUV420P;
const STREAM_DURATION: i64 = 10;

#[macro_export]
macro_rules! arrow {
    ($base:path, $field:ident) => {{
        #[allow(unused_unsafe)]
        unsafe {
            (*$base.as_ptr()).$field
        }
    }};
}

#[macro_export]
macro_rules! arrow_mut {
    ($base:path, $field:ident) => {{
        #[allow(unused_unsafe)]
        unsafe {
            (*$base.as_mut_ptr()).$field
        }
    }};
}

fn fill_yuv_image(pict: &mut frame::video::Video, frame_index: i64, width: usize, height: usize) {
    let i = frame_index;
    let linesize = unsafe { (*pict.as_ptr()).linesize };

    for y in 0..height {
        for x in 0..width {
            pict.data_mut(0)[y * linesize[0] as usize + x] = ((x + y) as i64 + i * 3) as u8;
        }
    }

    for y in 0..height / 2 {
        for x in 0..width / 2 {
            pict.data_mut(1)[y * linesize[1] as usize + x] = ((128 + y) as i64 + i * 2) as u8;
            pict.data_mut(2)[y * linesize[2] as usize + x] = ((64 + x) as i64 + i * 5) as u8;
        }
    }
}

fn write_frame(
    output: &mut format::context::Output,
    encoder: &mut codec::encoder::Encoder,
    // (stream_index, st_tb): (usize, Rational),
    stream: *mut sys::AVStream,
    frame: &util::frame::Frame,
) -> Result<(), util::error::Error> {
    encoder.send_frame(frame)?;
    let enc_tb = unsafe { arrow!(encoder, time_base) };

    loop {
        let mut packet = codec::packet::Packet::empty();

        match encoder.receive_packet(&mut packet) {
            Ok(_) => {}
            Err(util::error::Error::Other {
                errno: util::error::EAGAIN,
            })
            | Err(util::error::Error::Eof) => return Ok(()),
            Err(e) => panic!("Error on video recording with: {}", e),
        }

        let stream_time_base = unsafe { (*stream).time_base };
        unsafe { sys::av_packet_rescale_ts(packet.as_mut_ptr(), enc_tb, stream_time_base) };
        packet.set_stream(unsafe { (*stream).index } as usize);

        packet.write_interleaved(output)?;
    }
}

#[derive(Debug, Clone, Copy)]
struct VideoParams {
    fps: i32,
    width: u32,
    height: u32,
    bit_rate: usize,
}

fn alloc_picture(format: format::Pixel, width: u32, height: u32) -> frame::Video {
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

fn prepare_video_encoder<'a>(
    output: &'a mut format::context::Output,
    video_params: &VideoParams,
) -> Result<(encoder::video::Video, codec::Video, StreamMut<'a>), util::error::Error> {
    let format = output.format();
    let video_codec_id = unsafe { arrow!(format, video_codec) };

    let codec = codec::encoder::find(video_codec_id.into())
        .unwrap()
        .video()?;

    let mut stream = output.add_stream(codec)?;

    let context = codec::Context::new();
    let mut encoder = context.encoder().video()?;

    unsafe {
        (*encoder.as_mut_ptr()).codec_id = video_codec_id;
    }

    encoder.set_bit_rate(video_params.bit_rate);
    encoder.set_width(video_params.width);
    encoder.set_height(video_params.height);

    stream.set_time_base((1, video_params.fps));

    encoder.set_time_base(stream.time_base());

    encoder.set_gop(12);
    encoder.set_format(STREAM_FORMAT);
    if video_codec_id == Id::MPEG2VIDEO.into() {
        encoder.set_max_b_frames(2);
    }
    if video_codec_id == Id::MPEG1VIDEO.into() {
        encoder.set_mb_decision(encoder::Decision::RateDistortion);
    }

    // :v
    unsafe {
        (*encoder.as_mut_ptr()).flags |= codec::Flags::GLOBAL_HEADER.bits() as i32;
    }

    Ok((encoder, codec, stream))
}

struct VideoStream {
    encoder: encoder::Encoder,
    stream: *mut sys::AVStream,

    frame: frame::Video,
    tmp_frame: frame::Video,

    sws_context: software::scaling::Context,
}

impl VideoStream {
    fn open(
        encoder: encoder::video::Video,
        codec: codec::Video,
        stream: *mut sys::AVStream,
        options: Dictionary,
    ) -> Result<Self, util::error::Error> {
        let encoder = encoder.open_as_with(codec, options)?;

        let frame = alloc_picture(encoder.format(), encoder.width(), encoder.height());
        let tmp_frame = alloc_picture(
            format::pixel::Pixel::YUV420P,
            encoder.width(),
            encoder.height(),
        );

        let sws_context = software::scaling::Context::get(
            format::Pixel::YUV420P,
            encoder.width(),
            encoder.height(),
            encoder.format(),
            encoder.width(),
            encoder.height(),
            software::scaling::flag::Flags::BICUBIC,
        )?;

        // stream.set_parameters(&encoder);
        unsafe { sys::avcodec_parameters_from_context((*stream).codecpar, encoder.as_ptr()) };

        // let stream_info = (stream.index(), stream.time_base());

        Ok(Self {
            encoder: encoder.0 .0,
            frame,
            tmp_frame,
            stream,
            sws_context,
        })
    }
}

fn alloc_audio_frame(
    sample_format: format::Sample,
    channel_layout: util::channel_layout::ChannelLayout,
    sample_rate: u32,
    nb_samples: usize,
) -> frame::Audio {
    let mut frame = frame::Audio::empty();

    frame.set_rate(sample_rate);
    frame.set_format(sample_format);
    frame.set_samples(nb_samples);
    frame.set_channel_layout(channel_layout);

    unsafe {
        sys::av_frame_get_buffer(frame.as_mut_ptr(), 0);
    }

    frame
}

fn prepare_audio_encoder(
    output: &mut format::context::Output,
) -> Result<(encoder::audio::Audio, codec::Audio, StreamMut), util::error::Error> {
    let format = output.format();
    let audio_codec_id = unsafe { arrow!(format, audio_codec) };

    let mut codec = codec::encoder::find(audio_codec_id.into()).unwrap();

    let mut stream = output.add_stream(codec)?;
    let context = unsafe {
        let c = sys::avcodec_alloc_context3(codec.as_mut_ptr());
        codec::Context::wrap(c, None)
    };
    let mut encoder = context.encoder().audio()?;
    let codec = codec.audio()?;

    if let Some(mut sample_format) = codec.formats() {
        encoder.set_format(sample_format.next().unwrap());
    } else {
        encoder.set_format(format::Sample::F32(format::sample::Type::Planar));
    }
    encoder.set_bit_rate(64000);
    encoder.set_rate(if let Some(mut sample_rates) = codec.rates() {
        let mut rate = sample_rates.next().unwrap();
        for rates in sample_rates {
            if 44100 == rates {
                rate = 44100
            }
        }
        rate
    } else {
        44100
    });

    let channel_layout = if let Some(mut channel_layouts) = codec.channel_layouts() {
        let mut res = channel_layouts.next().unwrap();
        for layout in channel_layouts {
            if layout == ChannelLayout::STEREO {
                res = ChannelLayout::STEREO;
            }
        }
        res
    } else {
        ChannelLayout::STEREO
    };
    encoder.set_channel_layout(channel_layout);
    encoder.set_channels(channel_layout.channels());

    stream.set_time_base((1, encoder.rate() as i32));

    // :v
    unsafe {
        (*encoder.as_mut_ptr()).flags |= codec::Flags::GLOBAL_HEADER.bits() as i32;
    }

    Ok((encoder, codec, stream))
}
struct AudioStream {
    encoder: encoder::Encoder,
    stream: *mut sys::AVStream,
    // stream_info: (usize, Rational),
    samples_count: i64,

    t: f32,
    tincr: f32,
    tincr2: f32,

    frame: frame::Audio,
    tmp_frame: frame::Audio,

    swr_context: software::resampling::Context,
}

impl AudioStream {
    fn open(
        encoder: encoder::audio::Audio,
        codec: codec::Audio,
        stream: *mut sys::AVStream,
        options: Dictionary,
    ) -> Result<Self, util::error::Error> {
        let encoder = encoder.open_as_with(codec, options)?;

        let tincr = 2. * std::f32::consts::PI / encoder.rate() as f32;
        let tincr2 = 2. * std::f32::consts::PI / encoder.rate() as f32 / encoder.rate() as f32;

        let nb_samples = if codec
            .capabilities()
            .contains(codec::Capabilities::VARIABLE_FRAME_SIZE)
        {
            10_000
        } else {
            encoder.frame_size() as usize
        };

        let frame = alloc_audio_frame(
            encoder.format(),
            encoder.channel_layout(),
            encoder.rate(),
            nb_samples,
        );
        let tmp_frame = alloc_audio_frame(
            format::Sample::I16(format::sample::Type::Packed),
            encoder.channel_layout(),
            encoder.rate(),
            nb_samples,
        );

        // stream.set_parameters(&encoder);
        unsafe { sys::avcodec_parameters_from_context((*stream).codecpar, encoder.as_ptr()) };

        let swr_context = software::resampling::context::Context::get(
            format::Sample::I16(format::sample::Type::Packed),
            encoder.channel_layout(),
            encoder.rate(),
            encoder.format(),
            encoder.channel_layout(),
            encoder.rate(),
        )?;

        Ok(Self {
            encoder: encoder.0 .0,
            stream,
            samples_count: 0,
            t: 0.,
            tincr,
            tincr2,
            frame,
            tmp_frame,
            swr_context,
        })
    }
}

struct OutputStream<T> {
    encoder: T,

    next_pts: i64,
    logging_enabled: bool,
    frame_count: usize,
    last_log_frame_count: usize,
    starting_time: Instant,
    last_log_time: Instant,
}

impl OutputStream<VideoStream> {
    fn get_frame(&mut self) -> Option<&frame::Video> {
        let enc = &self.encoder.encoder;
        let time_base = unsafe { arrow!(enc, time_base) };
        let width = unsafe { arrow!(enc, width) } as usize;
        let height = unsafe { arrow!(enc, height) } as usize;
        if unsafe {
            sys::av_compare_ts(
                self.next_pts,
                time_base,
                STREAM_DURATION,
                Rational::from((1, 1)).into(),
            )
        } > 0
        {
            return None;
        }

        unsafe { sys::av_frame_make_writable(self.encoder.frame.as_mut_ptr()) }; // FIXME: Handle err

        fill_yuv_image(&mut self.encoder.tmp_frame, self.next_pts, width, height);
        self.encoder
            .sws_context
            .run(&self.encoder.tmp_frame, &mut self.encoder.frame)
            .unwrap();

        self.next_pts += 1;
        self.encoder.frame.set_pts(Some(self.next_pts));

        Some(&self.encoder.frame)
    }

    fn write_frame(
        &mut self,
        output: &mut format::context::Output,
    ) -> Result<(), util::error::Error> {
        if let Some(ref frame) = self.get_frame() {
            let frame = unsafe { util::frame::Frame::wrap(frame.as_ptr() as _) };
            write_frame(
                output,
                &mut self.encoder.encoder,
                self.encoder.stream,
                &frame,
            )?;
        }

        Ok(())
    }
}

// use util::mathematics::Rescale;

impl OutputStream<AudioStream> {
    fn get_frame(&mut self) -> Option<&frame::Audio> {
        let enc = &self.encoder.encoder;
        let time_base = unsafe { arrow!(enc, time_base) };
        let channels = unsafe { arrow!(enc, channels) };
        let num_samples = self.encoder.tmp_frame.samples();
        let data = self.encoder.tmp_frame.data_mut(0);
        if unsafe {
            sys::av_compare_ts(
                self.next_pts,
                time_base,
                STREAM_DURATION,
                Rational::from((1, 1)).into(),
            )
        } > 0
        {
            return None;
        }

        let mut index = 0;
        for _ in 0..num_samples {
            let v = self.encoder.t.sin() * 10_000.;
            for _ in 0..channels {
                data[index] = v as u8;
                index += 1;
            }
            self.encoder.t += self.encoder.tincr;
            self.encoder.tincr += self.encoder.tincr2;
        }

        self.next_pts += 1;
        self.encoder.frame.set_pts(Some(self.next_pts));

        Some(&self.encoder.frame)
    }

    fn write_frame(
        &mut self,
        output: &mut format::context::Output,
    ) -> Result<(), util::error::Error> {
        // let enc = &self.encoder.encoder;
        // let rate = unsafe { arrow!(enc, sample_rate) };

        // let frame = self.get_frame();

        // if let Some(mut f) = frame {
        //     let delay =
        //         unsafe { sys::swr_get_delay(self.encoder.swr_context.as_ptr() as _, rate as i64) }
        //             + f.samples() as i64;
        //     let dst_num_samples =
        //         delay.rescale_with(rate as f64, rate as f64, ffmpeg::Rounding::Up);

        //     unsafe { sys::av_frame_make_writable(f.as_mut_ptr()) }; // FIXME: Handle err

        //     self.encoder
        //         .swr_context
        //         .run(f, &mut self.encoder.frame)
        //         .unwrap();

        //     f = &self.encoder.frame;
        //     let time_base = unsafe { arrow!(enc, time_base) };
        //     unsafe {
        //         arrow_mut!(f, pts) = self
        //             .encoder
        //             .samples_count
        //             .rescale((1, rate), Rational::from(time_base));
        //     };

        //     self.encoder.samples_count += dst_num_samples;
        // }
        if let Some(ref frame) = self.get_frame() {
            let frame = unsafe { util::frame::Frame::wrap(frame.as_ptr() as _) };
            write_frame(
                output,
                &mut self.encoder.encoder,
                self.encoder.stream,
                &frame,
            )?
        }
        Ok(())
    }
}

impl<T> OutputStream<T> {
    fn new(encoder: T, logging_enabled: bool) -> Self {
        Self {
            encoder,
            next_pts: 0,
            logging_enabled,
            frame_count: 0,
            last_log_frame_count: 0,
            starting_time: Instant::now(),
            last_log_time: Instant::now(),
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ffmpeg::init()?;

    let filename = Path::new("out.mp4");
    let x264_opts = parse_opts(DEFAULT_X264_OPTS.to_string()).unwrap();

    let mut output = Rc::new(format::output(&filename).unwrap());

    let video_params = VideoParams {
        fps: 25,
        width: 352,
        height: 288,
        bit_rate: 400_000,
    };

    let (video_encoder, video_codec, mut video_stream) =
        prepare_video_encoder(Rc::get_mut(&mut output).unwrap(), &video_params)?;
    let video_stream_ptr = unsafe { video_stream.as_mut_ptr() };

    let (audio_encoder, audio_codec, mut audio_stream) =
        prepare_audio_encoder(Rc::get_mut(&mut output).unwrap())?;
    let audio_stream_ptr = unsafe { audio_stream.as_mut_ptr() };

    let mut video_stream = OutputStream::new(
        VideoStream::open(
            video_encoder,
            video_codec,
            video_stream_ptr,
            x264_opts.clone(),
        )?,
        true,
    );
    let mut audio_stream = OutputStream::new(
        AudioStream::open(
            audio_encoder,
            audio_codec,
            audio_stream_ptr,
            x264_opts.clone(),
        )?,
        true,
    );

    Rc::get_mut(&mut output).unwrap().write_header()?;

    let (mut encode_video, mut encode_audio) = (true, true);
    while encode_audio || encode_video {
        if encode_video
            && (!encode_audio
                || unsafe {
                    sys::av_compare_ts(
                        video_stream.next_pts,
                        (*video_stream.encoder.encoder.as_ptr()).time_base,
                        audio_stream.next_pts,
                        (*audio_stream.encoder.encoder.as_ptr()).time_base,
                    ) <= 0
                })
        {
            encode_video = video_stream
                .write_frame(Rc::get_mut(&mut output).unwrap())
                .is_ok();
        } else {
            encode_audio = audio_stream
                .write_frame(Rc::get_mut(&mut output).unwrap())
                .is_ok();
        }
    }

    Rc::get_mut(&mut output).unwrap().write_trailer()?;
    Ok(())
}
