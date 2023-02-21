use xmltree::Element;

#[derive(Debug)]
pub struct FileHeaderChunk {
    pub version: f32,
    pub xml: Element,
}

//TODO: check what fields are and are not really mandatory.
#[derive(Debug)]
pub struct StreamHeaderChunkInfo<Format> {
    pub name: String,
    pub r#type: String, // "type" is obviously a reserved keyword but can be escaped using r#
    pub channel_count: u32,
    pub nominal_srate: f64,
    pub channel_format: Format,
    pub created_at: f64,
    pub desc: Element,
}

#[derive(Debug)]
pub struct StreamHeaderChunk<Format> {
    pub stream_id: u32,
    pub info: StreamHeaderChunkInfo<Format>,
    pub xml: Element,
}

#[derive(Debug)]
pub struct Sample<T> {
    pub timestamp: Option<f64>,
    pub values: Vec<T>,
}

#[derive(Debug)]
pub struct SamplesChunk<'a, T> {
    pub stream_id: u32,
    pub samples: Vec<&'a Sample<T>>,
}

#[derive(Debug)]
// TODO: check whether or not these offsets can be negative
pub struct ClockOffsetChunk {
    pub stream_id: u32,
    pub collection_time: u64,
    pub offset_value: u64,
}

#[derive(Debug)]
pub struct BoundaryChunk {}

//TODO: check what fields are and are not really mandatory. If we don't have
//first or last timestamps given, is it ok if we instead determine those
//ourselves?
#[derive(Debug)]
pub struct StreamFooterChunkInfo {
    pub first_timestamp: Option<f64>,
    pub last_timestamp: Option<f64>,
    pub sample_count: u64,
    pub measured_srate: f64,
}

#[derive(Debug)]
pub struct StreamFooterChunk {
    pub stream_id: u32,
    pub info: StreamFooterChunkInfo,
    pub xml: Element,
}

#[derive(Debug)]
pub enum Tag {
    FileHeader,
    StreamHeader,
    Samples,
    ClockOffset,
    Boundary,
    StreamFooter,
}

// TODO: ensure correct visibility
#[derive(Debug)]
pub struct RawChunk {
    pub tag: Tag,
    pub content_bytes: Vec<u8>,
}