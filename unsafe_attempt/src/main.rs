use ffmpeg_next::util::error::Error as AVError;
use ffmpeg_sys_next as sys;
use sys::*;

use std::{mem::zeroed, ptr};

const STREAM_FRAME_RATE: i32 = 25;
const STREAM_PIX_FMT: AVPixelFormat = AVPixelFormat::AV_PIX_FMT_YUV420P;
const STREAM_DURATION: i64 = 10;
const SCALE_FLAGS: i32 = SWS_BICUBIC;

#[macro_export]
macro_rules! avcheck {
    ($e:expr) => {{
        match $e {
            e if e < 0 => Err(AVError::from(e)),
            _ => Ok($e),
        }
    }};
}

#[macro_export]
macro_rules! cstr {
    ($s:expr) => {
        concat!($s, "\0") as *const str as *const ::std::os::raw::c_char
    };
}

struct OutputStream {
    st: *mut sys::AVStream,
    enc: *mut sys::AVCodecContext,

    next_pts: i64,
    samples_count: i32,

    frame: *mut sys::AVFrame,
    tmp_frame: *mut sys::AVFrame,

    t: f32,
    tincr: f32,
    tincr2: f32,

    sws_ctx: *mut sys::SwsContext,
    swr_ctx: *mut sys::SwrContext,
}

fn log_packet(fmt_ctx: *const sys::AVFormatContext, pkt: *const sys::AVPacket) {
    unsafe {
        let stream_index = (*pkt).stream_index as isize;
        let time_base = (*(*(*fmt_ctx).streams.offset(stream_index))).time_base;

        let pts = (*pkt).pts;
        let dts = (*pkt).dts;
        let duration = (*pkt).duration;

        println!(
            "pts: {}, pts_time: {:.4}, dts: {}, dts_time: {:.4}, duration: {}, stream_index: {}, time_base: {}/{}",
            pts, av_q2d(time_base) * pts as f64, dts, av_q2d(time_base) * dts as f64,
            duration, stream_index, time_base.num, time_base.den
        );
    }
}

unsafe fn write_frame(
    fmt_ctx: *mut sys::AVFormatContext,
    c: *mut sys::AVCodecContext,
    st: *mut sys::AVStream,
    frame: *mut sys::AVFrame,
) -> Result<bool, AVError> {
    avcheck!(sys::avcodec_send_frame(c, frame))?;

    let mut ret = 0;
    while ret >= 0 {
        let mut pkt: AVPacket = zeroed();
        ret = sys::avcodec_receive_packet(c, &mut pkt);

        if ret == AVERROR(EAGAIN) || ret == AVERROR_EOF {
            break;
        }
        avcheck!(ret)?;

        av_packet_rescale_ts(&mut pkt, (*c).time_base, (*st).time_base);
        pkt.stream_index = (*st).index;

        log_packet(fmt_ctx, &pkt);
        avcheck!(av_interleaved_write_frame(fmt_ctx, &mut pkt))?;
        av_packet_unref(&mut pkt);
    }

    Ok(ret == AVERROR_EOF)
}

unsafe fn add_stream(
    ost: &mut OutputStream,
    oc: *mut AVFormatContext,
    codec: *mut *mut AVCodec,
    codec_id: AVCodecID,
) {
    *codec = avcodec_find_encoder(codec_id);
    if (*codec).is_null() {
        panic!("Could not find encoder for {:?}", codec_id);
    }

    ost.st = avformat_new_stream(oc, ptr::null());
    if ost.st.is_null() {
        panic!("Could not allocate stream");
    }
    (*ost.st).id = (*oc).nb_streams as i32 - 1;

    let c = avcodec_alloc_context3(*codec);
    if c.is_null() {
        panic!("Could not alloc an encoding context");
    }
    ost.enc = c;

    match (**codec).type_ {
        AVMediaType::AVMEDIA_TYPE_AUDIO => {
            (*c).sample_fmt = if (*(*codec)).sample_fmts.is_null() {
                AVSampleFormat::AV_SAMPLE_FMT_FLTP
            } else {
                *(*(*codec)).sample_fmts.offset(0)
            };
            (*c).bit_rate = 64000;
            (*c).sample_rate = 44100;

            (*c).channel_layout = AV_CH_LAYOUT_STEREO;
            (*c).channels = av_get_channel_layout_nb_channels((*c).channel_layout);
            (*(*ost).st).time_base = AVRational {
                num: 1,
                den: (*c).sample_rate,
            };
        }
        AVMediaType::AVMEDIA_TYPE_VIDEO => {
            (*c).codec_id = codec_id;
            (*c).bit_rate = 400_000;
            (*c).width = 352;
            (*c).height = 288;

            let time_base = AVRational {
                num: 1,
                den: STREAM_FRAME_RATE,
            };
            (*(*ost).st).time_base = time_base;
            (*c).time_base = (*(*ost).st).time_base;

            (*c).gop_size = 12;
            (*c).pix_fmt = STREAM_PIX_FMT;
            if (*c).codec_id == AVCodecID::AV_CODEC_ID_MPEG2VIDEO {
                (*c).max_b_frames = 2;
            }

            if (*c).codec_id == AVCodecID::AV_CODEC_ID_MPEG1VIDEO {
                (*c).mb_decision = 2;
            }
        }
        _ => {}
    }

    let flags = (*(*oc).oformat).flags;
    let mask = AVFMT_GLOBALHEADER;
    if (flags & mask) != 0 {
        (*c).flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
    }
}

unsafe fn alloc_audio_frame(
    sample_fmt: AVSampleFormat,
    channel_layout: u64,
    sample_rate: i32,
    nb_samples: i32,
) -> Result<*mut AVFrame, AVError> {
    let frame = av_frame_alloc();

    if frame.is_null() {
        panic!("Error allocating an audio frame");
    }

    (*frame).format = sample_fmt as i32;
    (*frame).channel_layout = channel_layout;
    (*frame).sample_rate = sample_rate;
    (*frame).nb_samples = nb_samples;

    if nb_samples != 0 {
        avcheck!(av_frame_get_buffer(frame, 0))?;
    }

    Ok(frame)
}

unsafe fn open_audio(
    codec: *mut AVCodec,
    ost: &mut OutputStream,
    opt_arg: *mut AVDictionary,
) -> Result<(), AVError> {
    // AVCodecContext *c;
    let nb_samples;
    let ret;
    let opt = ptr::null_mut();

    let c = ost.enc;

    /* open it */
    av_dict_copy(opt, opt_arg, 0);
    avcheck!(avcodec_open2(c, codec, opt))?;
    // av_dict_free(opt);

    /* init signal generator */
    (*ost).t = 0.;
    (*ost).tincr = 2. * std::f32::consts::PI * 110.0 / (*c).sample_rate as f32;
    /* increment frequency by 110 Hz per second */
    (*ost).tincr2 =
        2. * std::f32::consts::PI * 110.0 / (*c).sample_rate as f32 / (*c).sample_rate as f32;

    let flags = (*(*c).codec).capabilities as u32;
    let mask = AV_CODEC_CAP_VARIABLE_FRAME_SIZE;
    if flags & mask == 1 {
        nb_samples = 10000;
    } else {
        nb_samples = (*c).frame_size;
    }

    (*ost).frame = alloc_audio_frame(
        (*c).sample_fmt,
        (*c).channel_layout,
        (*c).sample_rate,
        nb_samples,
    )?;
    (*ost).tmp_frame = alloc_audio_frame(
        AVSampleFormat::AV_SAMPLE_FMT_S16,
        (*c).channel_layout,
        (*c).sample_rate,
        nb_samples,
    )?;

    avcheck!(avcodec_parameters_from_context((*(*ost).st).codecpar, c))?;

    (*ost).swr_ctx = swr_alloc();
    if (*ost).swr_ctx.is_null() {
        panic!("Could not allocate resampler context\n");
    }

    /* set options */
    av_opt_set_int(
        (*ost).swr_ctx as _,
        cstr!("in_channel_count"),
        (*c).channels as _,
        0,
    );
    av_opt_set_int(
        (*ost).swr_ctx as _,
        cstr!("in_sample_rate"),
        (*c).sample_rate as _,
        0,
    );
    av_opt_set_sample_fmt(
        (*ost).swr_ctx as _,
        cstr!("in_sample_fmt"),
        AVSampleFormat::AV_SAMPLE_FMT_S16 as _,
        0,
    );
    av_opt_set_int(
        (*ost).swr_ctx as _,
        cstr!("out_channel_count"),
        (*c).channels as _,
        0,
    );
    av_opt_set_int(
        (*ost).swr_ctx as _,
        cstr!("out_sample_rate"),
        (*c).sample_rate as _,
        0,
    );
    av_opt_set_sample_fmt(
        (*ost).swr_ctx as _,
        cstr!("out_sample_fmt"),
        (*c).sample_fmt,
        0,
    );

    /* initialize the resampling context */
    ret = swr_init((*ost).swr_ctx);
    if ret < 0 {
        panic!("Failed to initialize the resampling context\n");
    }

    Ok(())
}

unsafe fn get_audio_frame(ost: &mut OutputStream) -> *mut AVFrame {
    let frame = (*ost).tmp_frame;
    // int j, i, v;
    let q = (*frame).data[0];

    /* check if we want to generate more frames */
    if av_compare_ts(
        (*ost).next_pts,
        (*(*ost).enc).time_base,
        STREAM_DURATION,
        AVRational { num: 1, den: 1 },
    ) > 0
    {
        return ptr::null_mut();
    }

    let mut index = 0;
    for _j in 0..(*frame).nb_samples {
        let v = ost.t.sin() as i32 * 10000;
        for _i in 0..(*ost.enc).channels {
            q.offset(index).write(v as u8);
            index += 1;
        }
        ost.t += ost.tincr;
        ost.tincr += ost.tincr2;
    }

    (*frame).pts = (*ost).next_pts;
    ost.next_pts += (*frame).nb_samples as i64;

    frame
}

unsafe fn write_audio_frame(
    oc: *mut AVFormatContext,
    ost: &mut OutputStream,
) -> Result<bool, AVError> {
    let dst_nb_samples;

    let c = ost.enc;

    let mut frame = get_audio_frame(ost);

    if !frame.is_null() {
        /* convert samples from native format to destination codec format, using the
         * resampler */
        /* compute destination number of samples */
        dst_nb_samples = av_rescale_rnd(
            swr_get_delay((*ost).swr_ctx, (*c).sample_rate as i64) + (*frame).nb_samples as i64,
            (*c).sample_rate as i64,
            (*c).sample_rate as i64,
            AVRounding::AV_ROUND_UP,
        );
        // av_assert0(dst_nb_samples == (*frame).nb_samples);

        /* when we pass a frame to the encoder, it may keep a reference to it
         * internally;
         * make sure we do not overwrite it here
         */
        avcheck!(av_frame_make_writable((*ost).frame))?;

        /* convert to destination format */
        let ret = swr_convert(
            (*ost).swr_ctx,
            (*(*ost).frame).data.as_mut_ptr(),
            dst_nb_samples as i32,
            (*frame).data.as_mut_ptr() as _,
            (*frame).nb_samples,
        );
        if ret < 0 {
            panic!("Error while converting\n");
        }
        frame = (*ost).frame;

        (*frame).pts = av_rescale_q(
            (*ost).samples_count as i64,
            AVRational {
                num: 1,
                den: (*c).sample_rate,
            },
            (*c).time_base,
        );
        (*ost).samples_count += dst_nb_samples as i32;
    }

    write_frame(oc, c, (*ost).st, frame)
}

unsafe fn alloc_picture(pix_fmt: AVPixelFormat, width: i32, height: i32) -> *mut AVFrame {
    let picture = av_frame_alloc();
    if picture.is_null() {
        return ptr::null_mut();
    }

    (*picture).format = pix_fmt as i32;
    (*picture).width = width;
    (*picture).height = height;

    /* allocate the buffers for the frame data */
    let ret = av_frame_get_buffer(picture, 0);
    if ret < 0 {
        panic!("Could not allocate frame data.\n");
    }

    picture
}

unsafe fn open_video(
    codec: *mut AVCodec,
    ost: &mut OutputStream,
    opt_arg: *mut AVDictionary,
) -> Result<(), AVError> {
    let c = ost.enc;
    let opt = ptr::null_mut();

    av_dict_copy(opt, opt_arg, 0);

    /* open the codec */
    avcheck!(avcodec_open2(c, codec, opt))?;
    // av_dict_free(opt);

    /* allocate and init a re-usable frame */
    (*ost).frame = alloc_picture((*c).pix_fmt, (*c).width, (*c).height);
    if (*ost).frame.is_null() {
        panic!("Could not allocate video frame\n");
    }

    /* If the output format is not YUV420P, then a temporary YUV420P
     * picture is needed too. It is then converted to the required
     * output format. */
    (*ost).tmp_frame = ptr::null_mut();
    if (*c).pix_fmt != AVPixelFormat::AV_PIX_FMT_YUV420P {
        (*ost).tmp_frame =
            alloc_picture(AVPixelFormat::AV_PIX_FMT_YUV420P, (*c).width, (*c).height);
        if (*ost).tmp_frame.is_null() {
            panic!("Could not allocate temporary picture\n");
        }
    }

    /* copy the stream parameters to the muxer */
    let ret = avcodec_parameters_from_context((*ost.st).codecpar, c);
    if ret < 0 {
        panic!("Could not copy the stream parameters\n");
    }

    Ok(())
}

unsafe fn fill_yuv_image(pict: *mut AVFrame, frame_index: i64, width: i32, height: i32) {
    let i = frame_index as usize;

    /* Y */
    for y in 0..height as usize {
        for x in 0..width as usize {
            (*pict).data[0]
                .add(y * (*pict).linesize[0] as usize + x)
                .write((x + y + i * 3) as u8);
        }
    }

    /* Cb and Cr */
    for y in 0..height as usize / 2 {
        for x in 0..width as usize / 2 {
            (*pict).data[1]
                .add(y * (*pict).linesize[1] as usize + x)
                .write((128 + y + i * 2) as u8);
            (*pict).data[2]
                .add(y * (*pict).linesize[2] as usize + x)
                .write((64 + x + i * 5) as u8);
        }
    }
}

unsafe fn get_video_frame(ost: &mut OutputStream) -> *mut AVFrame {
    let c = ost.enc;

    /* check if we want to generate more frames */
    if av_compare_ts(
        (*ost).next_pts,
        (*c).time_base,
        STREAM_DURATION,
        AVRational { num: 1, den: 1 },
    ) > 0
    {
        return ptr::null_mut();
    }

    /* when we pass a frame to the encoder, it may keep a reference to it
     * internally; make sure we do not overwrite it here */
    if av_frame_make_writable((*ost).frame) < 0 {
        panic!("make_writeable")
    }

    if (*c).pix_fmt != AVPixelFormat::AV_PIX_FMT_YUV420P {
        /* as we only generate a YUV420P picture, we must convert it
         * to the codec pixel format if needed */
        if (*ost).sws_ctx.is_null() {
            (*ost).sws_ctx = sws_getContext(
                (*c).width,
                (*c).height,
                AVPixelFormat::AV_PIX_FMT_YUV420P,
                (*c).width,
                (*c).height,
                (*c).pix_fmt,
                SCALE_FLAGS,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            if (*ost).sws_ctx.is_null() {
                panic!("Could not initialize the conversion context\n");
            }
        }
        fill_yuv_image((*ost).tmp_frame, (*ost).next_pts, (*c).width, (*c).height);
        sws_scale(
            (*ost).sws_ctx,
            (*(*ost).tmp_frame).data.as_ptr() as _,
            (*(*ost).tmp_frame).linesize.as_ptr(),
            0,
            (*c).height,
            (*(*ost).frame).data.as_ptr(),
            (*(*ost).frame).linesize.as_ptr(),
        );
    } else {
        fill_yuv_image((*ost).frame, (*ost).next_pts, (*c).width, (*c).height);
    }

    ost.next_pts += 1;
    (*ost.frame).pts = ost.next_pts;

    ost.frame
}

unsafe fn write_video_frame(
    oc: *mut AVFormatContext,
    ost: &mut OutputStream,
) -> Result<bool, ffmpeg_next::Error> {
    write_frame(oc, ost.enc, ost.st, get_video_frame(ost))
}

unsafe fn close_stream(ost: &mut OutputStream) {
    avcodec_free_context(&mut (*ost).enc);
    av_frame_free(&mut (*ost).frame);
    av_frame_free(&mut (*ost).tmp_frame);
    sws_freeContext((*ost).sws_ctx);
    swr_free(&mut (*ost).swr_ctx);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let mut video_stream = zeroed();
        let mut audio_stream = zeroed();
        let mut oc: *mut AVFormatContext = zeroed();
        let mut video_codec = zeroed();
        let mut audio_codec = zeroed();
        let mut have_video = false;
        let mut encode_video = false;
        let mut have_audio = false;
        let mut encode_audio = false;
        let opt = zeroed();

        let filename = cstr!("out.mp4");

        avformat_alloc_output_context2(&mut oc, ptr::null_mut(), ptr::null_mut(), filename);
        if oc.is_null() {
            println!("Cannot deduce output format deom file extention; using MP4");
            avformat_alloc_output_context2(&mut oc, ptr::null_mut(), cstr!("mpeg"), filename);
        }
        if oc.is_null() {
            panic!("Bad file extention");
        }

        let fmt = (*oc).oformat;

        if (*fmt).video_codec != AVCodecID::AV_CODEC_ID_NONE {
            add_stream(&mut video_stream, oc, &mut video_codec, (*fmt).video_codec);
            have_video = true;
            encode_video = true;
        }
        if (*fmt).audio_codec != AVCodecID::AV_CODEC_ID_NONE {
            add_stream(&mut audio_stream, oc, &mut audio_codec, (*fmt).audio_codec);
            have_audio = true;
            encode_audio = true;
        }

        if have_video {
            open_video(video_codec, &mut video_stream, opt)?;
        }
        if have_audio {
            open_audio(audio_codec, &mut audio_stream, opt)?;
        }

        av_dump_format(oc, 0, filename, 1);

        let flags = (*fmt).flags;
        let mask = AVFMT_NOFILE;
        if flags & mask == 0 {
            let ret = avio_open(&mut (*oc).pb, filename, AVIO_FLAG_WRITE);
            if ret < 0 {
                panic!("Could not open '{:?}': {}\n", filename, ret);
            }
        }

        // avcheck!(avformat_write_header(oc, &mut opt))?;
        avformat_write_header(oc, ptr::null_mut());

        while encode_audio || encode_video {
            if encode_video
                && (!encode_audio
                    || av_compare_ts(
                        video_stream.next_pts,
                        (*video_stream.enc).time_base,
                        audio_stream.next_pts,
                        (*audio_stream.enc).time_base,
                    ) <= 0)
            {
                encode_video = !write_video_frame(oc, &mut video_stream)?;
            } else {
                encode_audio = !write_audio_frame(oc, &mut audio_stream)?;
            }
        }

        avcheck!(av_write_trailer(oc))?;

        if have_video {
            close_stream(&mut video_stream);
        }
        if have_audio {
            close_stream(&mut audio_stream);
        }

        let flags = (*fmt).flags;
        let mask = AVFMT_NOFILE;
        if flags & mask == 0 {
            avio_closep(&mut (*oc).pb);
        }

        /* free the stream */
        avformat_free_context(oc);
    }

    Ok(())
}
