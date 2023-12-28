use byteorder::{ByteOrder, LittleEndian};
use error_chain::bail;
use log::warn;
use xmltree::Element;

use core::slice;
use std::{collections::HashMap, io::Read};

use crate::{
    chunk_structs::*,
    errors::*,
    util::{extract_timestamp, get_text_from_child},
    Format, Sample, Values,
};

// this saves us some allocations.
// Unfortunately there is no cleaner way than a loop that I know of.
macro_rules! exact_byte_slice {
    ($iter:expr, $len:expr) => {{
        let mut slice = [0; $len];
        for i in 0..$len {
            match $iter.next() {
                Some(value) => slice[i] = value,
                None => bail!(ErrorKind::ReadChunkError),
            }
        }
        slice
    }};
}

pub(crate) fn read_to_raw_chunks(file_bytes: &[u8]) -> Result<Vec<RawChunk>> {
    let mut raw_chunks: Vec<RawChunk> = Vec::new();
    let mut file_header_found: bool = false;

    let mut content_iter = file_bytes.iter().copied();

    let magic_number = "XDF:".as_bytes();
    let file_start: Vec<u8> = content_iter.by_ref().take(magic_number.len()).collect();

    if file_start != magic_number {
        bail!(ErrorKind::NoMagicNumberError);
    }

    while let Some(num_length_bytes) = content_iter.next() {
        let chunk_length: usize = match num_length_bytes {
            1 => content_iter.next().unwrap() as usize,
            4 => {
                let length_bytes: [u8; 4] = exact_byte_slice!(content_iter.by_ref(), 4);
                LittleEndian::read_u32(&length_bytes) as usize
            }
            8 => {
                let length_bytes: [u8; 8] = exact_byte_slice!(content_iter.by_ref(), 8);
                LittleEndian::read_u64(&length_bytes) as usize
            }
            _ => {
                bail!(ErrorKind::InvalidNumCountBytes(num_length_bytes));
            }
        };

        // we could use take() here but this way saves us an allocation
        let tag_bytes: [u8; 2] = exact_byte_slice!(content_iter.by_ref(), 2);

        let chunk_tag_num = LittleEndian::read_u16(&tag_bytes);

        let chunk_tag: Tag = match chunk_tag_num {
            1 => {
                if file_header_found {
                    // more than one FileHeader found
                    warn!("Invalid: more than one FileHeader found. Returning what has been read so far.");
                    return Ok(raw_chunks);
                }
                file_header_found = true;
                Tag::FileHeader
            }
            2 => Tag::StreamHeader,
            3 => Tag::Samples,
            4 => Tag::ClockOffset,
            5 => Tag::Boundary,
            6 => Tag::StreamFooter,
            _ => bail!(ErrorKind::InvalidTagError(chunk_tag_num)),
        };

        //subtract the two tag bytes for the content length
        let remaining_chunk_length = chunk_length - 2;

        let chunk_bytes: Vec<u8> = content_iter.by_ref().take(remaining_chunk_length).collect();

        if chunk_bytes.len() != remaining_chunk_length {
            // File ended before chunk was finished.
            warn!("File ended mid-chunk, something is likely corrupted. Returning what has been read so far.");
            return Ok(raw_chunks);
        }

        let raw_chunk = RawChunk {
            tag: chunk_tag,
            content_bytes: chunk_bytes,
        };

        raw_chunks.push(raw_chunk);
    }

    if !file_header_found {
        bail!(ErrorKind::MissingFileHeaderError);
    }

    Ok(raw_chunks)
}

// yes these are ugly, they were extracted by refactoring
#[inline]
pub(crate) fn parse_stream_footer(raw_chunk: RawChunk) -> Result<Chunk> {
    let id_bytes = &raw_chunk.content_bytes[..4];
    let stream_id: u32 = LittleEndian::read_u32(id_bytes);
    let root = {
        match Element::parse(&raw_chunk.content_bytes[4..]) {
            Ok(root) => root,
            Err(err) => Err(err).chain_err(|| ErrorKind::ParseChunkError)?,
        }
    };

    let stream_footer_chunk = Chunk::StreamFooter(StreamFooterChunk { stream_id, xml: root });
    Ok(stream_footer_chunk)
}

#[inline]
pub(crate) fn parse_samples(
    raw_chunk: RawChunk,
    stream_num_samples_map: &mut HashMap<u32, u64>,
    stream_id: u32,
    stream_info_map: &HashMap<u32, StreamHeaderChunkInfo>,
) -> Result<Chunk> {
    let num_samples_byte_num = &raw_chunk.content_bytes[4];

    match num_samples_byte_num {
        1 | 4 | 8 => (),
        n => bail!(ErrorKind::InvalidNumCountBytes(*n)),
    }

    let num_samples_bytes = &raw_chunk.content_bytes[5..(5 + num_samples_byte_num) as usize];
    let num_samples: u64 = LittleEndian::read_uint(num_samples_bytes, *num_samples_byte_num as usize);

    stream_num_samples_map
        .entry(stream_id)
        .and_modify(|e| *e += num_samples)
        .or_insert(num_samples);

    let stream_info = stream_info_map
        .get(&stream_id)
        .ok_or(ErrorKind::MissingStreamHeaderError(stream_id))?;

    let type_size: Option<i32> = match stream_info.channel_format {
        Format::Int8 => Some(1),
        Format::Int16 => Some(2),
        Format::Int32 => Some(4),
        Format::Int64 => Some(8),
        Format::Float32 => Some(4),
        Format::Float64 => Some(8),
        Format::String => None,
    };

    let mut offset: usize = 4 + 1 + *num_samples_byte_num as usize;
    let mut samples: Vec<Sample> = Vec::with_capacity(num_samples as usize);
    if let Some(type_size) = type_size {
        //constant size types
        for _ in 0..num_samples {
            // let mut values: Vec<Value> = Vec::with_capacity(stream_info.channel_count as usize);
            let timestamp: Option<f64> = extract_timestamp(&raw_chunk, &mut offset);

            // realign the whole slice directly
            let values_bytes =
                &raw_chunk.content_bytes[offset..offset + (type_size as usize * stream_info.channel_count as usize)];

            // this macro is used to cast many values at once instead of iterating over them as this is much faster
            // the core issue is that we need to cast a slice of bytes to a slice of something else. This requires alignment.
            macro_rules! cast_values {
                ($type:ty, $variant:path) => {{
                    // create a vector of the correct type to get an aligned pointer
                    let mut vec_for_alignment: Vec<$type> =
                        vec![0 as $type; values_bytes.len() / std::mem::size_of::<$type>()];

                    // cast the &mut [$type] to a &mut [u8]
                    let vec_for_alignment_as_bytes =
                        bytemuck::cast_slice_mut::<$type, u8>(vec_for_alignment.as_mut_slice());

                    // copy the read bytes into the vector slice
                    vec_for_alignment_as_bytes.copy_from_slice(values_bytes);

                    // cast the byte slice back into the correct type
                    let vals = bytemuck::cast_slice::<u8, $type>(vec_for_alignment_as_bytes).to_vec();

                    $variant(vals)
                }};
            }

            let values: Values = match stream_info.channel_format {
                Format::Int8 => {
                    let vals = bytemuck::cast_slice::<u8, i8>(values_bytes).to_vec();

                    Values::Int8(vals)
                }
                Format::Int16 => cast_values!(i16, Values::Int16),
                Format::Int32 => cast_values!(i32, Values::Int32),
                Format::Int64 => cast_values!(i64, Values::Int64),
                Format::Float32 => cast_values!(f32, Values::Float32),
                Format::Float64 => cast_values!(f64, Values::Float64),

                Format::String => unreachable!(),
            };
            offset += type_size as usize * stream_info.channel_count as usize;

            samples.push(Sample { timestamp, values });
        }
    } else {
        //strings
        for _ in 0..num_samples {
            let timestamp: Option<f64> = extract_timestamp(&raw_chunk, &mut offset);
            let num_length_bytes = raw_chunk.content_bytes[offset];
            offset += 1; //for number of length bytes field
            let value_length = match num_length_bytes {
                1 => raw_chunk.content_bytes[offset] as u64,
                4 => u32::from_le_bytes(
                    (&raw_chunk.content_bytes[offset..(offset + num_length_bytes as usize)]).try_into()?,
                ) as u64,
                8 => u64::from_le_bytes(
                    (&raw_chunk.content_bytes[offset..(offset + num_length_bytes as usize)]).try_into()?,
                ),
                n => bail!(ErrorKind::InvalidNumCountBytes(n)),
            } as usize;
            offset += num_length_bytes as usize; // for length field
            let mut value_bytes = &raw_chunk.content_bytes[offset..(offset + value_length)];

            // Turn the bytes into a valid utf-8 string
            let mut value_string = String::new();
            value_bytes.read_to_string(&mut value_string)?;

            samples.push(Sample {
                timestamp,
                values: Values::String(value_string),
            });
            offset += value_length; // for value field
        }
    }
    let samples_chunk = Chunk::Samples(SamplesChunk { stream_id, samples });
    Ok(samples_chunk)
}

#[inline]
pub(crate) fn parse_stream_header(
    raw_chunk: &RawChunk,
    stream_info_map: &mut HashMap<u32, StreamHeaderChunkInfo>,
) -> Result<Chunk> {
    let id_bytes = &raw_chunk.content_bytes[..4];
    let stream_id: u32 = LittleEndian::read_u32(id_bytes);
    let root = Element::parse(&raw_chunk.content_bytes[4..])?;

    let info = StreamHeaderChunkInfo {
        name: Some(get_text_from_child(&root, "name")?),
        r#type: Some(get_text_from_child(&root, "type")?),
        channel_count: get_text_from_child(&root, "channel_count")?
            .parse()
            .chain_err(|| ErrorKind::BadXMLElementError("channel_count".to_string()))?,
        nominal_srate: Some(
            get_text_from_child(&root, "nominal_srate")?
                .parse()
                .chain_err(|| ErrorKind::BadXMLElementError("nominal_srate".to_string()))?,
        ),
        channel_format: match get_text_from_child(&root, "channel_format")?.to_lowercase().as_str() {
            "in8" => Format::Int8,
            "int16" => Format::Int16,
            "int32" => Format::Int32,
            "int64" => Format::Int64,
            "float32" => Format::Float32,
            "double64" => Format::Float64,
            "string" => Format::String,
            invalid => bail!(Error::from(format!("Invalid stream channel format \"{}\"", invalid))
                .chain_err(|| ErrorKind::BadXMLElementError("channel_format".to_string()))),
        },
    };
    stream_info_map.insert(stream_id, info.clone());
    let stream_header_chunk = StreamHeaderChunk {
        stream_id,
        info,
        xml: root,
    };
    Ok(Chunk::StreamHeader(stream_header_chunk))
}

// tests
#[test]
fn empty_file() {
    let bytes: Vec<u8> = vec![];
    let res = read_to_raw_chunks(bytes.as_slice());
    let err = res.unwrap_err();
    assert!(
        matches!(err, Error(ErrorKind::NoMagicNumberError, _)),
        "Expected NoMagicNumberError, got {:?}",
        err
    );
}

#[test]
fn no_magic_num() {
    let bytes: Vec<u8> = vec![b'X', b'D', b'A', b':'];
    let res = read_to_raw_chunks(bytes.as_slice());
    let err = res.unwrap_err();
    assert!(
        matches!(err, Error(ErrorKind::NoMagicNumberError, _)),
        "Expected NoMagicNumberError, got {:?}",
        err
    );
}

#[test]
fn chunk_too_short() {
    // magic number, then a Samples chunk with specified length of length 20 but insufficient actual length
    let bytes: Vec<u8> = vec![b'X', b'D', b'F', b':', 4, 0, 0, 0, 20, 3, 0, 1, 2, 3];
    let res = read_to_raw_chunks(bytes.as_slice());
    let chunks = res.unwrap();
    assert_eq!(chunks.len(), 0);
}

#[test]
fn invalid_tags() {
    //tag 0 is invalid
    let bytes: Vec<u8> = vec![b'X', b'D', b'F', b':', 1, 3, 0, 0, 10];
    let res = read_to_raw_chunks(bytes.as_slice());
    let err = res.unwrap_err();
    assert!(
        matches!(err, Error(ErrorKind::InvalidTagError(invalid_tag), _) if invalid_tag == 0),
        "Expected InvalidTagError(0), got {:?}",
        err
    );

    //tag 7 is invalid
    let bytes: Vec<u8> = vec![b'X', b'D', b'F', b':', 1, 3, 7, 0, 10];
    let res = read_to_raw_chunks(bytes.as_slice());
    let err = res.unwrap_err();
    assert!(
        matches!(err, Error(ErrorKind::InvalidTagError(invalid_tag), _) if invalid_tag == 7),
        "Expected InvalidTagError(7), got {:?}",
        err
    );
}
