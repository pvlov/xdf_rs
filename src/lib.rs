#![forbid(unsafe_code)]
#![forbid(clippy::unwrap_used)]
#![deny(nonstandard_style)]
#![warn(array_into_iter)]
#![warn(missing_docs)]
#![warn(rustdoc::all)]
#![deny(clippy::pedantic)]
#![allow(clippy::cast_precision_loss)] // this is only relevant if you have 2^52 or more samples in a single chunk. 2^52 bytes would be over 4 petabytes.
#![crate_type = "lib"]
#![crate_name = "xdf"]

//! [![github]](https://github.com/Garfield100/xdf_rs)
//! [![crates]](https://crates.io/crates/xdf)
//!
//! [github]: https://img.shields.io/badge/github-9090ff?style=for-the-badge&logo=github&labelColor=555555
//! [crates]: https://img.shields.io/badge/crates.io-fc8d62?style=for-the-badge&labelColor=555555&logo=rust
//!
//! Read (and maybe one day write) XDF files
//! Currently the only supported XDF version is 1.0. (at the time of writing, this the only version that exists)
//!
//! [`XDF format specification`]: https://github.com/sccn/xdf/wiki/Specifications
//!
//! This library provides a way to read files in the [`XDF format`] as specified by SCCN.
//!
//! # Example
//! ```rust
//!# use std::fs;
//!# use xdf::XDFFile;
//!# fn main() -> Result<(), Box<dyn std::error::Error>> {
//!let bytes = fs::read("tests/minimal.xdf")?;
//!let xdf_file = XDFFile::from_bytes(&bytes)?;
//!# Ok(())
//!# }
//!```

mod chunk_structs;
mod errors;
mod parsers;
mod sample;
mod streams;
mod util;

use log::warn;
use std::collections::HashMap;
use std::iter::Iterator;
use std::sync::Arc;

pub use errors::XDFError;
pub use sample::Sample;
pub use streams::Stream;

use chunk_structs::{BoundaryChunk, ClockOffsetChunk, FileHeaderChunk, StreamFooterChunk, StreamHeaderChunk};
use errors::{ParseError, StreamError};
use util::FiniteF64;

use crate::chunk_structs::Chunk;
use crate::parsers::xdf_file::xdf_file_parser;

type StreamID = u32;
type SampleIter = std::vec::IntoIter<Sample>;

/// XDF file struct
/// The main struct representing an XDF file.
#[derive(Debug, Clone, PartialEq)]
pub struct XDFFile {
    /// XDF version. Currently only 1.0 exists according to the specification.
    pub version: f32,
    /// The XML header of the XDF file as an [`xmltree::Element`].
    pub header: xmltree::Element,
    /// A vector of streams contained in the XDF file.
    pub streams: Vec<Stream>,
}

/// Possible formats for the data in a stream as given in the specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// signed 8-bit integer
    Int8,
    /// signed 16-bit integer
    Int16,
    /// signed 32-bit integer
    Int32,
    /// signed 64-bit integer
    Int64,
    /// 32-bit floating point number
    Float32,
    /// 64-bit floating point number
    Float64,
    /// UTF-8 encoded string, for example for event markers.
    String,
}

/// The values of a sample in a stream. The values are stored as a vector of the corresponding type (or a string).
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq)]
pub enum Values {
    Int8(Vec<i8>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Float32(Vec<f32>),
    Float64(Vec<f64>),
    String(String),
}

struct GroupedChunks {
    stream_header_chunks: Vec<StreamHeaderChunk>,
    stream_footer_chunks: Vec<StreamFooterChunk>,
    clock_offsets: HashMap<StreamID, Vec<ClockOffsetChunk>>,
    sample_map: HashMap<StreamID, Vec<SampleIter>>,
}

impl XDFFile {
    /**
    Parse an XDF file from a byte slice.
    # Arguments
    * `bytes` - A byte slice of the whole XDF file as read from disk.
    # Returns
    * A Result containing the parsed [`XDFFile`] or an [`XDFError`]
    # Errors
    Will error if the file could not be parsed correctly for various reasons. See [`XDFError`] for more information.
    # Example
    ```rust
    # use std::fs;
    # use xdf::XDFFile;
    # fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read("tests/minimal.xdf")?;
    let xdf_file = XDFFile::from_bytes(&bytes)?;
    # Ok(())
    # }
    ```
    */
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, XDFError> {
        // this error mapping could use some simplification
        let (input, chunks) = xdf_file_parser(bytes)
            .map_err(|e| match e {
                // we have to map the error to use Arc instead of slice because we would otherwise need a static lifetime.
                nom::Err::Incomplete(n) => nom::Err::Incomplete(n),
                nom::Err::Error(nom::error::Error { input, code }) => nom::Err::Error(nom::error::Error {
                    input: Arc::from(input.to_owned()),
                    code,
                }),
                nom::Err::Failure(nom::error::Error { input, code }) => nom::Err::Failure(nom::error::Error {
                    input: Arc::from(input.to_owned()),
                    code,
                }),
            })
            .map_err(ParseError::from)?;

        // we don't error here to be more error tolerant and allow for partial parsing
        if !input.is_empty() {
            warn!("There are {} bytes left in the input after parsing.", input.len());
        }

        let (file_header_chunk, grouped_chunks) = group_chunks(chunks)?;

        let streams = process_streams(grouped_chunks)?;

        Ok(Self {
            version: file_header_chunk.version,
            header: file_header_chunk.xml,
            streams,
        })
    }
}

// takes a vector of chunks and sorts them into a GroupedChunks struct based on their type
fn group_chunks(chunks: Vec<Chunk>) -> Result<(FileHeaderChunk, GroupedChunks), XDFError> {
    let mut file_header_chunk: Option<FileHeaderChunk> = None;
    let mut stream_header_chunks: Vec<StreamHeaderChunk> = Vec::new();
    let mut stream_footer_chunks: Vec<StreamFooterChunk> = Vec::new();
    let mut clock_offsets: HashMap<StreamID, Vec<ClockOffsetChunk>> = HashMap::new();

    // the sample_map maps stream IDs to a vector of iterators which each iterate over one chunk's samples
    let sample_map = chunks
        .into_iter()
        .filter_map(|chunk_res| {
            match chunk_res {
                Chunk::FileHeader(c) => {
                    file_header_chunk = Some(c);
                    None
                }
                Chunk::StreamHeader(c) => {
                    stream_header_chunks.push(c);
                    None
                }
                Chunk::StreamFooter(c) => {
                    stream_footer_chunks.push(c);
                    None
                }
                Chunk::Samples(c) => Some(c), // pass only samples through to the fold
                Chunk::ClockOffset(c) => {
                    clock_offsets.entry(c.stream_id).or_default().push(c);

                    None
                }
                Chunk::Boundary(_) => None, // boundary chunks are discarded for now
            }
        })
        .fold(
            // fold the samples into a map of stream IDs to a vector of iterators so we can merge them later
            HashMap::new(),
            |mut map: HashMap<StreamID, Vec<SampleIter>>, chunk| {
                map.entry(chunk.stream_id).or_default().push(chunk.samples.into_iter());
                map
            },
        );

    let file_header_chunk = file_header_chunk.ok_or(StreamError::MissingFileHeader)?;

    let info = GroupedChunks {
        stream_header_chunks,
        stream_footer_chunks,
        clock_offsets,
        sample_map,
    };

    // yes I return these separately. It saves me a clone. Sue me.
    Ok((file_header_chunk, info))
}

// takes grouped chunks and combines them into finished streams.
fn process_streams(mut grouped_chunks: GroupedChunks) -> Result<Vec<Stream>, XDFError> {
    let stream_header_map: HashMap<StreamID, StreamHeaderChunk> = grouped_chunks
        .stream_header_chunks
        .into_iter()
        .map(|s| (s.stream_id, s))
        .collect();

    let mut stream_footer_map: HashMap<StreamID, StreamFooterChunk> = grouped_chunks
        .stream_footer_chunks
        .into_iter()
        .map(|s| (s.stream_id, s))
        .collect();

    // this can happen if the recording stops unexpectedly.
    // We allow this to be more error tolerant and not lose all experimental data.
    for &stream_id in stream_header_map.keys() {
        if !stream_footer_map.contains_key(&stream_id) {
            warn!("Stream header without corresponding stream footer for id: {stream_id}");
        }
    }

    // this on the other hand is a bit weirder but again, we allow it to be more error tolerant
    for &stream_id in stream_footer_map.keys() {
        if !stream_header_map.contains_key(&stream_id) {
            warn!("Stream footer without corresponding stream header for id: {stream_id}");
        }
    }

    let mut streams_vec: Vec<Stream> = Vec::new();

    for (stream_id, stream_header) in stream_header_map {
        let stream_footer = stream_footer_map.remove(&stream_id);

        let name = stream_header.info.name.as_ref().map(|name| Arc::from(name.as_str()));

        let stream_type = stream_header
            .info
            .stream_type
            .as_ref()
            .map(|stream_type| Arc::from(stream_type.as_str()));

        let mut stream_offsets = grouped_chunks
            .clock_offsets
            .remove(&stream_header.stream_id)
            .unwrap_or_default();

        // Since clock offsets are internal types only, I could look into usinng a FiniteF64 type.
        stream_offsets.retain(|o| o.collection_time.is_finite() && o.offset_value.is_finite());

        if !stream_offsets.is_sorted() {
            return Err(ParseError::InvalidClockOffset.into());
        }

        let samples_vec: Vec<Sample> = process_samples(
            grouped_chunks.sample_map.remove(&stream_id).unwrap_or_default(),
            &stream_offsets,
            stream_header.info.nominal_srate,
        );

        let measured_srate = if stream_header.info.nominal_srate.is_some() {
            // nominal_srate is given as "a floating point number in Hertz. If the stream
            // has an irregular sampling rate (that is, the samples are not spaced evenly in
            // time, for example in an event stream), this value must be 0."
            // we use None instead of 0.

            let first_timestamp: Option<f64> = samples_vec.first().and_then(|s| s.timestamp);
            let last_timestamp: Option<f64> = samples_vec.last().and_then(|s| s.timestamp);

            if let (Some(first_timestamp), Some(last_timestamp)) = (first_timestamp, last_timestamp) {
                let delta = last_timestamp - first_timestamp;
                if delta <= 0.0 || !delta.is_finite() {
                    None // don't divide by zero :)
                } else {
                    Some(samples_vec.len() as f64 / delta)
                }
            } else {
                None
            }
        } else {
            None
        };

        let stream = Stream {
            id: stream_id,
            channel_count: stream_header.info.channel_count,
            nominal_srate: stream_header.info.nominal_srate,
            format: stream_header.info.channel_format,

            name,
            r#type: stream_type,
            header: stream_header.xml,
            footer: stream_footer.map(|s| s.xml),
            measured_srate,
            samples: samples_vec,
        };

        streams_vec.push(stream);
    }

    Ok(streams_vec)
}

/// takes a bunch of iterators over a stream's samples and some offsets and
/// combines them into a vector of samples with timestamps corrected by interpolated clock offsets.
fn process_samples(
    mut sample_iterators: Vec<SampleIter>,
    stream_offsets: &[ClockOffsetChunk],
    nominal_srate: Option<f64>,
) -> Vec<Sample> {
    debug_assert!(stream_offsets
        .iter()
        .all(|o| o.stream_id == stream_offsets[0].stream_id));

    let mut offset_index: usize = 0;

    let mut most_recent_timestamp = (0_usize, 0_f64);

    // Sort the iterators according to first timestamp.
    // If the first sample from this iterator has no timestamp, append this iterator to the previous iterator
    // What if the first sample from the first iterator also has no timestamp?
    // Both the Python and the Matlab implementations use zero as a first default, so I've done the same here.

    let mut sample_iterators_merged = vec![];
    if let Some((first, rest)) = sample_iterators.split_first_mut() {
        // We store each set of iterators with the first iter's first timestamp in a tuple
        let mut first = first.peekable();
        let first_ts = first
            .peek()
            .and_then(|s| s.timestamp)
            .and_then(FiniteF64::new)
            .unwrap_or(FiniteF64::zero());
        sample_iterators_merged.push((first_ts, vec![first]));

        for it in rest {
            let mut it = it.peekable();
            if let Some(first_sample) = it.peek() {
                // If there is a timestamp and it is finite, create a new set of iterators
                if let Some(ts) = first_sample.timestamp.and_then(FiniteF64::new) {
                    sample_iterators_merged.push((ts, vec![it]));
                } else {
                    // Technically this need not be checked as there is always a last
                    if let Some(v) = sample_iterators_merged.last_mut() {
                        v.1.push(it);
                    }
                }
            }
        }
    }

    // Now we have a vec of tuples containing a finite timestamp and a vec of iterators.
    // We need to sort the outer vec and chain the iterators in each inner vec.

    sample_iterators_merged.sort_by_key(|t| t.0);

    let sample_iterators = sample_iterators_merged
        .into_iter()
        .flat_map(|t| t.1)
        .map(Iterator::peekable)
        .filter_map(|mut it| if it.peek().is_none() { None } else { Some(it) });

    // let samples_in_order: bool = sample_iterators
    //     .clone()
    //     .filter_map(|mut it| it.peek().map(|s| s.timestamp))
    //     .is_sorted();

    // if !samples_in_order {
    //     return Err(XDFError::InvalidSample);
    // }

    sample_iterators
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(i, s)| -> Sample {
            if let Some(srate) = nominal_srate {
                let timestamp = if let Some(timestamp) = s.timestamp {
                    // if the sample has its own timestamp, use that and update the most recent timestamp
                    most_recent_timestamp = (i, timestamp);
                    s.timestamp
                } else {
                    // if this sample has no timestamp but a previous sample did, calculate this one's timestamp using the srate
                    let (old_i, old_timestamp) = most_recent_timestamp;
                    let samples_since_ts = i - old_i;
                    Some(old_timestamp + (samples_since_ts as f64 / srate))
                };

                let timestamp = timestamp.map(|ts| interpolate_and_add_offsets(ts, stream_offsets, &mut offset_index));

                Sample {
                    timestamp,
                    values: s.values,
                }
            } else {
                s
            }
        })
        .collect()
}

/// takes a timestamp and a vector of clock offsets and interpolates the offsets to find an offset for the timestamp.
/// the `offset_index` is used to keep track where to start looking for the right clock offsets.
fn interpolate_and_add_offsets(ts: f64, stream_offsets: &[ClockOffsetChunk], offset_index: &mut usize) -> f64 {
    if stream_offsets.is_empty() {
        ts //there are no offsets;
    } else {
        let time_or_nan = |i: usize| {
            stream_offsets
                .get(i + 1)
                .map_or(f64::NAN, |c: &ClockOffsetChunk| c.collection_time)
            //use NaN to break out of the loop below in case we've gone out of bounds
            // this avoids an infinite loop in the unusual case where all clock offsets are newer than the timestamp.
        };

        // if the current timestamp is older than the what the current offset would imply,
        // the offset must either be zero (and the timestamp older than *every* offset),
        // or something has gone horribly wrong (for example the clock offsets or the chunks are not in order of collection time).

        // indexing to zero is safe because we know the vector is not empty
        if ts < stream_offsets[0].collection_time {
            // debug_assert_eq!(
            //     *offset_index, 0,
            //     "Timestamp is older than the first clock offset, but the offset index is not zero."
            // );
            // I initially thought this would be an invalid state, however this can happen if the chunks are not in order of collection time.
            // This isn't great but not fatal either. We check clock offsets for being in order, so it can't be those.
            // As a best effort we add the first stream offset, as that is the closest one.
            return ts + stream_offsets[0].offset_value;
        }

        // ensure clock offset at offset_index is older than the current timestamp
        while ts > time_or_nan(*offset_index) {
            *offset_index += 1;
        }

        // get the most recent offset before the current timestamp
        let prev_offset = stream_offsets.get(*offset_index).or_else(|| stream_offsets.last());

        // and the clock offset which comes next
        let next_offset = stream_offsets.get(*offset_index + 1).or_else(|| stream_offsets.last());

        let interpolated = if let (Some(l), Some(n)) = (prev_offset, next_offset) {
            // nearly all cases will have to be interpolated
            // a * (1-x) + b * x (with x between 0 and 1 of course)

            let dt = n.collection_time - l.collection_time;

            // can be zero if the offsets are the same
            if dt > 0.0 {
                let t_normalised = (ts - l.collection_time) / dt;
                l.offset_value * (1.0 - t_normalised) + n.offset_value * t_normalised
            } else {
                l.offset_value
            }
        } else {
            prev_offset.or(next_offset).map_or(0.0, |c| c.offset_value)
        };

        ts + interpolated
    }
}

// TESTS

#[cfg(test)]
mod tests {

    use super::*;

    const EPSILON: f64 = 1E-14;

    // now without panics!
    #[test]
    fn test_interpolation_bad_offset() {
        let offsets = vec![
            ClockOffsetChunk {
                collection_time: 0.0,
                offset_value: -1.0,
                stream_id: 0,
            },
            ClockOffsetChunk {
                collection_time: 1.0,
                offset_value: 1.0,
                stream_id: 0,
            },
        ];
        // after the range we expect for the last offset to be used
        let first_offset = offsets.first().unwrap();
        let timestamp = first_offset.collection_time - 1.0;
        let mut offset_index = 1;

        // should panic
        interpolate_and_add_offsets(timestamp, &offsets, &mut offset_index);
    }

    // test the interpolation function for timestamps *inside* the range of offsets
    #[test]
    fn test_interpolation_inside() {
        const TEST_VALUES: [((f64, f64), (f64, f64)); 4] = [
            ((0.0, -1.0), (1.0, 1.0)),
            ((0.0, 0.0), (1.0, 1.0)),
            ((0.0, -1.0), (1.0, 5.0)),
            ((4.0, -1.0), (5.0, 2.0)),
        ];

        for ((s1_t, s1_v), (s2_t, s2_v)) in TEST_VALUES {
            let offsets = vec![
                ClockOffsetChunk {
                    collection_time: s1_t,
                    offset_value: s1_v,
                    stream_id: 0,
                },
                ClockOffsetChunk {
                    collection_time: s2_t,
                    offset_value: s2_v,
                    stream_id: 0,
                },
            ];

            let incline = (offsets[1].offset_value - offsets[0].offset_value)
                / (offsets[1].collection_time - offsets[0].collection_time);

            let first_pos = (
                offsets.first().unwrap().collection_time,
                offsets.first().unwrap().offset_value,
            );

            let linspace = |start: f64, end: f64, n: usize| {
                (0..n)
                    .map(|i| start + (end - start) * (i as f64) / (n as f64))
                    .collect::<Vec<f64>>()
            };

            // test at multiple steps
            for timestamp in linspace(s1_t, s2_t, 100) {
                let mut offset_index = 0;
                let interpolated = interpolate_and_add_offsets(timestamp, &offsets, &mut offset_index);

                let expected: f64 = timestamp + ((timestamp - first_pos.0) * incline + first_pos.1); // original timestamp + interpolated offset

                assert!(
                    (interpolated - expected).abs() < EPSILON,
                    "expected {interpolated} to be within {EPSILON} of {expected}"
                );
            }
        }
    }

    // test the interpolation function for timestamps after the last offset
    #[test]
    fn test_interpolation_after() {
        let offsets = vec![
            ClockOffsetChunk {
                collection_time: 0.0,
                offset_value: -1.0,
                stream_id: 0,
            },
            ClockOffsetChunk {
                collection_time: 1.0,
                offset_value: 1.0,
                stream_id: 0,
            },
            ClockOffsetChunk {
                collection_time: 3.0,
                offset_value: 2.0,
                stream_id: 0,
            },
        ];
        // after the range we expect for the last offset to be used
        let last_offset = offsets.last().unwrap();
        let timestamp = last_offset.collection_time + 1.0;
        let mut offset_index = 0;
        let interpolated = interpolate_and_add_offsets(timestamp, &offsets, &mut offset_index);
        let expected = timestamp + last_offset.offset_value;

        assert!(
            (interpolated - expected).abs() < EPSILON,
            "expected {interpolated} to be within {EPSILON} of {expected}"
        );
    }

    // test the interpolation function for timestamps before the first offset
    #[test]
    fn test_interpolation_before() {
        let offsets = vec![
            ClockOffsetChunk {
                collection_time: 0.0,
                offset_value: -1.0,
                stream_id: 0,
            },
            ClockOffsetChunk {
                collection_time: 1.0,
                offset_value: 1.0,
                stream_id: 0,
            },
        ];
        // after the range we expect for the last offset to be used
        let first_offset = offsets.first().unwrap();
        let timestamp = first_offset.collection_time - 1.0;
        let mut offset_index = 0;
        let interpolated = interpolate_and_add_offsets(timestamp, &offsets, &mut offset_index);
        let expected = timestamp + first_offset.offset_value;

        assert!(
            (interpolated - expected).abs() < EPSILON,
            "expected {interpolated} to be within {EPSILON} of {expected}"
        );
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn test_no_offsets() {
        let offsets = vec![];
        let mut offset_index = 0;

        for i in -20..=20 {
            let timestamp = f64::from(i) / 10.0;
            let res = interpolate_and_add_offsets(timestamp, &offsets, &mut offset_index);

            //should be unchanged
            assert_eq!(timestamp, res);
        }
    }

    #[test]
    const fn test_is_sync() {
        const fn is_sync<T: Sync>() {}
        is_sync::<XDFFile>();
    }

    #[test]
    const fn test_is_send() {
        const fn is_send<T: Send>() {}
        is_send::<XDFFile>();
    }
}
