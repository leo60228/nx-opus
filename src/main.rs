use anyhow::{anyhow, Context, Result};
use nom::branch::alt;
use nom::bytes::complete::{tag, take};
use nom::combinator::{iterator, map, value};
use nom::multi::length_data;
use nom::number::complete::*;
use nom::sequence::{preceded, terminated, tuple};
use nom::IResult;
use ogg::writing::*;
use std::env::args_os;
use std::fs;
use std::fs::File;
use std::io::prelude::*;

#[derive(Debug)]
pub struct Header {
    pub channel_count: u8,
    pub skip: u16,
    pub sample_rate: u32,
    pub data_offset: u32,
}

pub fn header(input: &[u8]) -> IResult<&[u8], Header> {
    map(
        tuple((
            tag(0x80000001u32.to_le_bytes()), // 0x00: magic
            take(5usize),                     // 0x04: skip 5 bytes
            le_u8,                            // 0x09: channel count
            take(2usize),                     // 0x0a: skip 2 bytes
            le_u32,                           // 0x0c: sample rate
            le_u32,                           // 0x10: data offset
            take(8usize),                     // 0x14: skip 8 bytes
            le_u16,                           // 0x1c: skip
        )),
        |(_, _, channel_count, _, sample_rate, data_offset, _, skip)| Header {
            channel_count,
            skip,
            sample_rate,
            data_offset,
        },
    )(input)
}

pub fn data_header(input: &[u8]) -> IResult<&[u8], u32> {
    preceded(tag(0x80000004u32.to_le_bytes()), le_u32)(input)
}

pub fn packet(input: &[u8]) -> IResult<&[u8], &[u8]> {
    length_data(terminated(be_u32, take(4usize)))(input)
}

pub fn write_id_header(writer: &mut impl Write, header: &Header) -> Result<()> {
    writer.write_all(b"OpusHead")?; // magic
    writer.write_all(&[0x01])?; // version 1
    writer.write_all(&[header.channel_count])?; // channels
    writer.write_all(&header.skip.to_le_bytes())?; // pre-skip
    writer.write_all(&header.sample_rate.to_le_bytes())?; // sample rate
    writer.write_all(&[0x00, 0x00])?; // gain
    writer.write_all(&[0x00])?; // mapping family 0

    Ok(())
}

pub const COMMENT_HEADER: &[u8] = b"OpusTags\x07\x00\x00\x00nx-opus\x00\x00\x00\x00";

#[derive(Debug)]
pub struct OpusPacket {
    pub config: u8,
    pub stereo: bool,
    pub frames: u8,
}

pub fn opus_packet(input: &[u8]) -> IResult<&[u8], OpusPacket> {
    use nom::bits::{bits, complete::*};

    bits(map::<_, _, _, nom::error::Error<_>, _, _>(
        tuple((
            take(5usize),
            map(take(1usize), |x: u8| x != 0),
            alt((
                value(1, tag(0usize, 2usize)),
                value(2, tag(1usize, 2usize)),
                value(2, tag(2usize, 2usize)),
                preceded(
                    tag(3usize, 2usize),
                    preceded(take::<_, u8, _, _>(2usize), take(6usize)),
                ),
            )),
        )),
        |(config, stereo, frames)| OpusPacket {
            config,
            stereo,
            frames,
        },
    ))(input)
}

pub fn frame_size(config: u8) -> u64 {
    const SILK: &[u64] = &[100, 200, 400, 600];
    const HYBRID: &[u64] = &[100, 200];
    const CELT: &[u64] = &[25, 50, 100, 200];

    let sizes = match config {
        0..=11 => SILK,
        12..=15 => HYBRID,
        16..=31 => CELT,
        _ => unreachable!(),
    };

    let idx = config as usize % sizes.len();

    sizes[idx]
}

fn main() -> Result<()> {
    let file = fs::read(args_os().nth(1).context("Missing path!")?)?;
    let out_file = File::create(args_os().nth(2).context("Missing path!")?)?;
    let mut writer = PacketWriter::new(out_file);

    let header = header(&file).map_err(|x| anyhow!("{}", x))?.1;
    dbg!(&header);

    let mut id_header: Vec<u8> = vec![];
    write_id_header(&mut id_header, &header)?;
    writer.write_packet(id_header.into(), 0, PacketWriteEndInfo::EndPage, 0)?;

    writer.write_packet(COMMENT_HEADER.into(), 0, PacketWriteEndInfo::EndPage, 0)?;

    let (data, length) =
        data_header(&file[header.data_offset as usize..]).map_err(|x| anyhow!("{}", x))?;

    dbg!(length);

    let mut iter = iterator(data, packet);

    let mut peekable = iter.into_iter().enumerate().peekable();

    let mut pos = 0;

    while let Some((i, packet)) = peekable.next() {
        let opus = opus_packet(packet).map_err(|x| anyhow!("{}", x))?.1;
        let size = frame_size(opus.config);
        let duration = 48000 * size / 10000;

        pos += duration;

        let end = if peekable.peek().is_none() {
            PacketWriteEndInfo::EndStream
        } else if (i + 1) % (header.channel_count as usize) == 0 {
            PacketWriteEndInfo::EndPage
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        writer.write_packet(packet.into(), 0, end, pos)?;
    }

    iter.finish().map_err(|x| anyhow!("{}", x))?;

    Ok(())
}
