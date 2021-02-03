use encoder::Encoder;
use ffmpeg::{
    codec::{self, codec::Codec, packet::Packet},
    encoder, format, packet, sys,
    util::{self, error, frame},
    Frame, Rational,
};
use ffmpeg_next as ffmpeg;
use format::Output;
use std::{fs::File, io::Write, path::Path};

fn test_ffmpeg() -> Result<(), Box<dyn std::error::Error>> {
    let codec = encoder::find(codec::Id::MPEG2VIDEO).unwrap();

    let context = codec::Context::new();
    let mut encoder = context.encoder().video()?;

    let mut packet = Packet::empty();

    encoder.set_bit_rate(400_000);
    encoder.set_width(352);
    encoder.set_height(288);
    encoder.set_time_base((1, 25));
    encoder.set_frame_rate(Some((25, 1)));
    encoder.set_gop(10); // 12
    encoder.set_max_b_frames(2);
    // codec.set_mb_decision(encoder::Decision::RateDistortion); //  MPEG1Video
    encoder.set_format(format::Pixel::YUV420P);

    let mut encoder = encoder.open_as(codec)?;

    let filename = Path::new("out.mp4");
    // let mut output = match format::output(&filename) {
    //     Ok(res) => res,
    //     Err(e) => {
    //         eprintln!(
    //             "Could not deduce output format from file extension {} with error: {}",
    //             filename.display(),
    //             e
    //         );
    //         format::output_as(&filename, "mpeg")?
    //     }
    // };

    let mut f = File::create(filename)?;
    let mut frame = alloc_picture(encoder.format(), encoder.width(), encoder.height());

    let linesize = unsafe { (*frame.as_ptr()).linesize };
    for i in 0..25 {
        println!("Frame: {}", i);
        assert!(unsafe { sys::av_frame_make_writable(frame.as_mut_ptr()) } >= 0);

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

        encode(&mut encoder, &frame, &mut packet, &mut f)?;
    }

    f.write_all(&[0, 0, 1, 0xb7]).unwrap();

    f.sync_all().unwrap();

    Ok(())
}

fn alloc_picture(format: format::Pixel, width: u32, height: u32) -> frame::video::Video {
    let mut frame = frame::video::Video::empty();
    unsafe {
        frame.alloc(format, width, height);
    }

    frame
}

fn encode(
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
