//! Zstandard decompression (RFC 8878).
//!
//! Codex compresses the transcripts of older threads into `.jsonl.zst` files. This decoder
//! reads the JSONL they hold without a dependency: it decodes Zstandard frames one after
//! another, passes over skippable frames, and checks each frame's declared content size and
//! content checksum when the frame carries them. A frame that needs a dictionary is refused.

use std::io::{self, BufReader, Read, Seek, SeekFrom};

const FRAME_MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
const SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
/// The most a block holds, compressed or decompressed.
const MAX_BLOCK_SIZE: usize = 128 * 1024;
/// The largest window read: how far back a frame's matches may reach.
const MAX_WINDOW_SIZE: u64 = 1 << 31;
/// Output that is read and older than the window is dropped once this much of it has built up.
const COMPACT_THRESHOLD: usize = 1 << 20;

/// The bytes a Zstandard stream decompresses to, read in order.
pub struct Decoder<R> {
    input: BufReader<R>,
    frame: Option<Frame>,
    /// The current frame's recent output: the window later matches copy from, followed by the
    /// bytes not read yet.
    output: Vec<u8>,
    /// Where the bytes not read yet start in `output`.
    unread: usize,
    /// A compressed block's bytes.
    block: Vec<u8>,
    /// A compressed block's literals.
    literals: Vec<u8>,
}

/// What a frame's blocks share.
struct Frame {
    window_size: usize,
    content_size: Option<u64>,
    checksum: Option<Xxh64>,
    produced: u64,
    repeat_offsets: [usize; 3],
    huffman: Option<HuffmanTable>,
    literal_lengths: Option<FseTable>,
    offsets: Option<FseTable>,
    match_lengths: Option<FseTable>,
}

struct FrameHeader {
    window_size: u64,
    content_size: Option<u64>,
    has_checksum: bool,
}

impl<R: Read> Decoder<R> {
    pub fn new(input: R) -> Self {
        Self {
            input: BufReader::new(input),
            frame: None,
            output: Vec::new(),
            unread: 0,
            block: Vec::new(),
            literals: Vec::new(),
        }
    }

    pub fn get_ref(&self) -> &R {
        self.input.get_ref()
    }

    /// Passes over `count` bytes of the decompressed stream, or what is left of it.
    pub fn skip(&mut self, count: u64) -> io::Result<u64> {
        let mut skipped = 0u64;
        while skipped < count {
            let available = self.output.len() - self.unread;
            if available == 0 {
                if !self.next_block()? {
                    break;
                }
                continue;
            }
            let step =
                usize::try_from(count - skipped).map_or(available, |left| left.min(available));
            self.unread += step;
            skipped += step as u64;
        }
        Ok(skipped)
    }

    /// Decodes the next block, opening a frame first when none is open: `false` once the input
    /// ends where a frame could start.
    fn next_block(&mut self) -> io::Result<bool> {
        if self.frame.is_none() && !self.open_frame()? {
            return Ok(false);
        }
        let frame = self.frame.as_mut().expect("a frame is open");
        let expired = self
            .output
            .len()
            .saturating_sub(frame.window_size)
            .min(self.unread);
        if expired >= COMPACT_THRESHOLD.max(frame.window_size) {
            self.output.drain(..expired);
            self.unread -= expired;
        }
        let header = read_le(&mut self.input, 3)?;
        let last = header & 1 == 1;
        let size = (header >> 3) as usize;
        if size > MAX_BLOCK_SIZE {
            return Err(corrupt("a block is larger than a block can be"));
        }
        let start = self.output.len();
        match (header >> 1) & 3 {
            0 => {
                self.output.resize(start + size, 0);
                read_exact(&mut self.input, &mut self.output[start..])?;
            }
            1 => {
                let byte = read_le(&mut self.input, 1)? as u8;
                self.output.resize(start + size, byte);
            }
            2 => {
                self.block.resize(size, 0);
                read_exact(&mut self.input, &mut self.block)?;
                let used = decode_literals(frame, &self.block, &mut self.literals)?;
                decode_sequences(frame, &self.block[used..], &self.literals, &mut self.output)?;
            }
            _ => return Err(corrupt("a block has the reserved type")),
        }
        let produced = &self.output[start..];
        frame.produced += produced.len() as u64;
        if frame
            .content_size
            .is_some_and(|declared| frame.produced > declared)
        {
            return Err(corrupt("a frame holds more than its declared size"));
        }
        if let Some(checksum) = frame.checksum.as_mut() {
            checksum.update(produced);
        }
        if last {
            self.close_frame()?;
        }
        Ok(true)
    }

    /// Reads the next frame's header, passing over skippable frames: `false` at the end of the
    /// input.
    fn open_frame(&mut self) -> io::Result<bool> {
        loop {
            let mut magic = [0u8; 4];
            match read_up_to(&mut self.input, &mut magic)? {
                0 => return Ok(false),
                4 => {}
                _ => return Err(corrupt("the data ends inside a frame")),
            }
            let magic = u32::from_le_bytes(magic);
            if magic & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC {
                let size = read_le(&mut self.input, 4)?;
                let skipped = io::copy(&mut (&mut self.input).take(size), &mut io::sink())?;
                if skipped < size {
                    return Err(corrupt("the data ends inside a skippable frame"));
                }
                continue;
            }
            if magic != FRAME_MAGIC {
                return Err(corrupt("the data is not Zstandard"));
            }
            let header = read_frame_header(&mut self.input)?;
            // A frame is opened once every byte before it is read, and its matches never reach
            // into the frame before it.
            debug_assert_eq!(self.unread, self.output.len());
            self.output.clear();
            self.unread = 0;
            self.frame = Some(Frame {
                window_size: header.window_size as usize,
                content_size: header.content_size,
                checksum: header.has_checksum.then(Xxh64::default),
                produced: 0,
                repeat_offsets: [1, 4, 8],
                huffman: None,
                literal_lengths: None,
                offsets: None,
                match_lengths: None,
            });
            return Ok(true);
        }
    }

    fn close_frame(&mut self) -> io::Result<()> {
        let frame = self.frame.take().expect("a frame is open");
        if frame
            .content_size
            .is_some_and(|declared| frame.produced != declared)
        {
            return Err(corrupt("a frame holds less than its declared size"));
        }
        if let Some(checksum) = frame.checksum {
            let stored = read_le(&mut self.input, 4)?;
            if stored != checksum.digest() & 0xFFFF_FFFF {
                return Err(corrupt("a frame's content checksum does not match"));
            }
        }
        Ok(())
    }
}

impl<R: Read + Seek> Decoder<R> {
    /// Starts again from the beginning of the input.
    pub fn rewind(&mut self) -> io::Result<()> {
        self.input.seek(SeekFrom::Start(0))?;
        self.frame = None;
        self.output.clear();
        self.unread = 0;
        Ok(())
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        while self.unread == self.output.len() {
            if !self.next_block()? {
                return Ok(0);
            }
        }
        let count = (self.output.len() - self.unread).min(buffer.len());
        buffer[..count].copy_from_slice(&self.output[self.unread..self.unread + count]);
        self.unread += count;
        Ok(count)
    }
}

/// The content size the first frame of a stream declares, if it declares one.
pub fn declared_content_size<R: Read>(input: &mut R) -> io::Result<Option<u64>> {
    if read_le(input, 4)? != u64::from(FRAME_MAGIC) {
        return Ok(None);
    }
    Ok(read_frame_header(input)?.content_size)
}

fn read_frame_header<R: Read>(input: &mut R) -> io::Result<FrameHeader> {
    let descriptor = read_le(input, 1)? as u8;
    if descriptor & 0x08 != 0 {
        return Err(corrupt("a frame header has its reserved bit set"));
    }
    let single_segment = descriptor & 0x20 != 0;
    let window_size = if single_segment {
        None
    } else {
        let window = read_le(input, 1)?;
        let base = 1u64 << (10 + (window >> 3));
        Some(base + base / 8 * (window & 7))
    };
    let dictionary_id = read_le(input, [0, 1, 2, 4][usize::from(descriptor & 3)])?;
    if dictionary_id != 0 {
        return Err(corrupt("a frame needs a dictionary"));
    }
    let content_size = match descriptor >> 6 {
        0 if single_segment => Some(read_le(input, 1)?),
        0 => None,
        1 => Some(read_le(input, 2)? + 256),
        2 => Some(read_le(input, 4)?),
        _ => Some(read_le(input, 8)?),
    };
    // A single-segment frame's window is its whole content.
    let window_size = window_size
        .or(content_size)
        .expect("a single-segment frame declares its size");
    if window_size > MAX_WINDOW_SIZE {
        return Err(corrupt("a frame's window is larger than ebira reads"));
    }
    Ok(FrameHeader {
        window_size,
        content_size,
        has_checksum: descriptor & 0x04 != 0,
    })
}

// Literals

/// Decodes a compressed block's literals section into `literals`, returning its length.
fn decode_literals(frame: &mut Frame, block: &[u8], literals: &mut Vec<u8>) -> io::Result<usize> {
    let first = *block
        .first()
        .ok_or_else(|| corrupt("a compressed block is empty"))?;
    let header = |count: usize| {
        block
            .get(..count)
            .map(|bytes| {
                bytes
                    .iter()
                    .rev()
                    .fold(0u64, |value, byte| value << 8 | u64::from(*byte))
            })
            .ok_or_else(|| corrupt("a literals section header is cut short"))
    };
    let size_format = (first >> 2) & 3;
    literals.clear();
    if first & 3 < 2 {
        // Raw or RLE literals: the size alone, in 5, 12, or 20 bits.
        let (size, header_size) = match size_format {
            0 | 2 => (usize::from(first >> 3), 1),
            1 => ((header(2)? >> 4) as usize, 2),
            _ => ((header(3)? >> 4) as usize, 3),
        };
        if size > MAX_BLOCK_SIZE {
            return Err(corrupt("a block has more literals than a block can hold"));
        }
        if first & 3 == 0 {
            let bytes = block
                .get(header_size..header_size + size)
                .ok_or_else(|| corrupt("raw literals are cut short"))?;
            literals.extend_from_slice(bytes);
            return Ok(header_size + size);
        }
        let byte = *block
            .get(header_size)
            .ok_or_else(|| corrupt("RLE literals are cut short"))?;
        literals.resize(size, byte);
        return Ok(header_size + 1);
    }
    // Huffman-coded literals, with a new table or the previous block's.
    let (size, compressed_size, header_size, streams) = match size_format {
        0 | 1 => {
            let value = header(3)?;
            (
                (value >> 4) & 0x3FF,
                (value >> 14) & 0x3FF,
                3,
                if size_format == 0 { 1 } else { 4 },
            )
        }
        2 => {
            let value = header(4)?;
            ((value >> 4) & 0x3FFF, (value >> 18) & 0x3FFF, 4, 4)
        }
        _ => {
            let value = header(5)?;
            ((value >> 4) & 0x3FFFF, (value >> 22) & 0x3FFFF, 5, 4)
        }
    };
    let (size, compressed_size) = (size as usize, compressed_size as usize);
    if size > MAX_BLOCK_SIZE {
        return Err(corrupt("a block has more literals than a block can hold"));
    }
    let end = header_size + compressed_size;
    let mut data = block
        .get(header_size..end)
        .ok_or_else(|| corrupt("compressed literals are cut short"))?;
    if first & 3 == 2 {
        let (table, used) = HuffmanTable::read(data)?;
        frame.huffman = Some(table);
        data = &data[used..];
    }
    let table = frame
        .huffman
        .as_ref()
        .ok_or_else(|| corrupt("literals reuse a Huffman table no earlier block defined"))?;
    literals.reserve(size);
    if streams == 1 {
        table.decode(data, size, literals)?;
        return Ok(end);
    }
    let jump = data
        .get(..6)
        .ok_or_else(|| corrupt("a literals jump table is cut short"))?;
    let lengths = [
        usize::from(u16::from_le_bytes([jump[0], jump[1]])),
        usize::from(u16::from_le_bytes([jump[2], jump[3]])),
        usize::from(u16::from_le_bytes([jump[4], jump[5]])),
    ];
    let segment = size.div_ceil(4);
    let last_segment = size
        .checked_sub(3 * segment)
        .ok_or_else(|| corrupt("too few literals for four streams"))?;
    let mut streams = &data[6..];
    for length in lengths {
        let stream = streams
            .get(..length)
            .ok_or_else(|| corrupt("a literals stream is cut short"))?;
        table.decode(stream, segment, literals)?;
        streams = &streams[length..];
    }
    table.decode(streams, last_segment, literals)?;
    Ok(end)
}

/// A Huffman decoding table: for each value of the next `max_bits` bits, the symbol whose code
/// they start with and the length of that code.
struct HuffmanTable {
    max_bits: u32,
    entries: Vec<(u8, u8)>,
}

impl HuffmanTable {
    /// Reads a Huffman tree description, returning the table and the description's length.
    fn read(data: &[u8]) -> io::Result<(Self, usize)> {
        let header = usize::from(
            *data
                .first()
                .ok_or_else(|| corrupt("a Huffman tree description is missing"))?,
        );
        let mut weights = Vec::with_capacity(256);
        let used = if header < 128 {
            let compressed = data
                .get(1..1 + header)
                .ok_or_else(|| corrupt("Huffman weights are cut short"))?;
            decode_weights(compressed, &mut weights)?;
            1 + header
        } else {
            let count = header - 127;
            let packed = data
                .get(1..1 + count.div_ceil(2))
                .ok_or_else(|| corrupt("Huffman weights are cut short"))?;
            weights.extend((0..count).map(|index| {
                let byte = packed[index / 2];
                if index % 2 == 0 {
                    byte >> 4
                } else {
                    byte & 15
                }
            }));
            1 + count.div_ceil(2)
        };
        Ok((Self::from_weights(&weights)?, used))
    }

    /// Builds the table from the weights of every symbol but the last, whose weight completes
    /// the code space.
    fn from_weights(weights: &[u8]) -> io::Result<Self> {
        if weights.len() > 255 {
            return Err(corrupt("a Huffman table has too many symbols"));
        }
        let mut total = 0u32;
        for &weight in weights {
            if weight > 11 {
                return Err(corrupt("a Huffman weight is out of range"));
            }
            if weight > 0 {
                total += 1 << (weight - 1);
            }
        }
        if total == 0 {
            return Err(corrupt("a Huffman table has no symbols"));
        }
        let max_bits = 32 - total.leading_zeros();
        let left = (1u32 << max_bits) - total;
        if max_bits > 11 || !left.is_power_of_two() {
            return Err(corrupt("Huffman weights do not complete a code"));
        }
        let mut all = weights.to_vec();
        all.push((left.trailing_zeros() + 1) as u8);
        // Codes go to symbols by increasing weight, in symbol order within a weight; a symbol
        // of weight w takes 2^(w-1) consecutive entries.
        let mut counts = [0usize; 13];
        for &weight in &all {
            counts[usize::from(weight)] += 1;
        }
        let mut starts = [0usize; 13];
        let mut position = 0usize;
        for (weight, (start, count)) in starts.iter_mut().zip(counts).enumerate().skip(1) {
            *start = position;
            position += count << (weight - 1);
        }
        let mut entries = vec![(0u8, 0u8); 1 << max_bits];
        for (symbol, &weight) in all.iter().enumerate() {
            if weight == 0 {
                continue;
            }
            let span = 1usize << (weight - 1);
            let start = starts[usize::from(weight)];
            entries[start..start + span]
                .fill((symbol as u8, (max_bits + 1 - u32::from(weight)) as u8));
            starts[usize::from(weight)] += span;
        }
        Ok(Self { max_bits, entries })
    }

    /// Decodes `count` symbols from one stream, which they must use up exactly.
    fn decode(&self, stream: &[u8], count: usize, out: &mut Vec<u8>) -> io::Result<()> {
        let mut bits = BackwardBits::new(stream)?;
        for _ in 0..count {
            let (symbol, length) = self.entries[bits.peek(self.max_bits) as usize];
            bits.consume(u32::from(length));
            out.push(symbol);
        }
        if !bits.finished() {
            return Err(corrupt(
                "a literals stream does not end with its last symbol",
            ));
        }
        Ok(())
    }
}

/// Decodes FSE-compressed Huffman weights: two states take turns until the bits run out.
fn decode_weights(data: &[u8], weights: &mut Vec<u8>) -> io::Result<()> {
    let (table, used) = FseTable::read(data, 255, 6)?;
    let mut bits = BackwardBits::new(&data[used..])?;
    let mut states = [bits.read(table.log) as usize, bits.read(table.log) as usize];
    if bits.overflowed() {
        return Err(corrupt("Huffman weights are cut short"));
    }
    for turn in 0.. {
        let (current, other) = (turn % 2, 1 - turn % 2);
        let entry = table.entries[states[current]];
        weights.push(entry.symbol);
        states[current] = usize::from(entry.base) + bits.read(u32::from(entry.bits)) as usize;
        if bits.overflowed() {
            weights.push(table.entries[states[other]].symbol);
            break;
        }
        if weights.len() > 255 {
            return Err(corrupt("a Huffman table has too many symbols"));
        }
    }
    Ok(())
}

// Sequences

/// One kind of sequence code: its largest symbol, the largest table accuracy a block may give
/// it, and its predefined distribution.
struct SequenceCode {
    max_symbol: usize,
    max_log: u32,
    default_log: u32,
    default: &'static [i32],
}

const LITERAL_LENGTHS: SequenceCode = SequenceCode {
    max_symbol: 35,
    max_log: 9,
    default_log: 6,
    default: &[
        4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1,
        1, 1, -1, -1, -1, -1,
    ],
};

const OFFSETS: SequenceCode = SequenceCode {
    max_symbol: 31,
    max_log: 8,
    default_log: 5,
    default: &[
        1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
    ],
};

const MATCH_LENGTHS: SequenceCode = SequenceCode {
    max_symbol: 52,
    max_log: 9,
    default_log: 6,
    default: &[
        1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
    ],
};

const LITERAL_LENGTH_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LITERAL_LENGTH_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
const MATCH_LENGTH_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const MATCH_LENGTH_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

/// Decodes a compressed block's sequences section and executes the sequences, appending the
/// block's output to `output`.
fn decode_sequences(
    frame: &mut Frame,
    data: &[u8],
    literals: &[u8],
    output: &mut Vec<u8>,
) -> io::Result<()> {
    let block_start = output.len();
    let (count, mut used) = match *data {
        [] => return Err(corrupt("a compressed block has no sequences section")),
        [0, ..] => (0, 1),
        [byte @ 1..=127, ..] => (usize::from(byte), 1),
        [byte @ 128..=254, second, ..] => ((usize::from(byte - 128) << 8) + usize::from(second), 2),
        [255, second, third, ..] => (usize::from(second) + (usize::from(third) << 8) + 0x7F00, 3),
        _ => return Err(corrupt("a sequences section header is cut short")),
    };
    if count == 0 {
        if used != data.len() {
            return Err(corrupt("bytes follow an empty sequences section"));
        }
        output.extend_from_slice(literals);
        return Ok(());
    }
    let modes = *data
        .get(used)
        .ok_or_else(|| corrupt("a sequences section header is cut short"))?;
    used += 1;
    if modes & 3 != 0 {
        return Err(corrupt("a sequences section sets reserved bits"));
    }
    let Frame {
        window_size,
        repeat_offsets,
        literal_lengths,
        offsets,
        match_lengths,
        ..
    } = frame;
    used += read_sequence_table(literal_lengths, modes >> 6, &data[used..], &LITERAL_LENGTHS)?;
    used += read_sequence_table(offsets, (modes >> 4) & 3, &data[used..], &OFFSETS)?;
    used += read_sequence_table(
        match_lengths,
        (modes >> 2) & 3,
        &data[used..],
        &MATCH_LENGTHS,
    )?;
    let literal_table = literal_lengths.as_ref().expect("the table was read");
    let offset_table = offsets.as_ref().expect("the table was read");
    let match_table = match_lengths.as_ref().expect("the table was read");

    let mut bits = BackwardBits::new(&data[used..])?;
    let mut literal_state = bits.read(literal_table.log) as usize;
    let mut offset_state = bits.read(offset_table.log) as usize;
    let mut match_state = bits.read(match_table.log) as usize;
    let mut literal_position = 0usize;
    for index in 0..count {
        let literal_entry = literal_table.entries[literal_state];
        let offset_entry = offset_table.entries[offset_state];
        let match_entry = match_table.entries[match_state];
        // The extra bits come in this order: offset, match length, literal length.
        let offset_code = u32::from(offset_entry.symbol);
        let offset_value = (1u64 << offset_code) + bits.read(offset_code);
        let match_code = usize::from(match_entry.symbol);
        let match_length = MATCH_LENGTH_BASE[match_code] as usize
            + bits.read(u32::from(MATCH_LENGTH_BITS[match_code])) as usize;
        let literal_code = usize::from(literal_entry.symbol);
        let literal_length = LITERAL_LENGTH_BASE[literal_code] as usize
            + bits.read(u32::from(LITERAL_LENGTH_BITS[literal_code])) as usize;
        let offset = resolve_offset(repeat_offsets, offset_value, literal_length)?;

        let literal_end = literal_position + literal_length;
        if literal_end > literals.len() {
            return Err(corrupt("a sequence uses more literals than its block has"));
        }
        if output.len() - block_start + literal_length + match_length > MAX_BLOCK_SIZE {
            return Err(corrupt(
                "a block decompresses to more than a block can hold",
            ));
        }
        output.extend_from_slice(&literals[literal_position..literal_end]);
        literal_position = literal_end;
        copy_match(output, offset, match_length, *window_size)?;

        if index + 1 < count {
            // The states advance in this order: literal length, match length, offset.
            literal_state =
                usize::from(literal_entry.base) + bits.read(u32::from(literal_entry.bits)) as usize;
            match_state =
                usize::from(match_entry.base) + bits.read(u32::from(match_entry.bits)) as usize;
            offset_state =
                usize::from(offset_entry.base) + bits.read(u32::from(offset_entry.bits)) as usize;
        }
    }
    if !bits.finished() {
        return Err(corrupt(
            "a sequences bitstream does not end with its last sequence",
        ));
    }
    let rest = &literals[literal_position..];
    if output.len() - block_start + rest.len() > MAX_BLOCK_SIZE {
        return Err(corrupt(
            "a block decompresses to more than a block can hold",
        ));
    }
    output.extend_from_slice(rest);
    Ok(())
}

/// Sets the table one kind of sequence code uses in this block, returning the bytes its
/// description took.
fn read_sequence_table(
    slot: &mut Option<FseTable>,
    mode: u8,
    data: &[u8],
    code: &SequenceCode,
) -> io::Result<usize> {
    match mode {
        0 => {
            *slot = Some(FseTable::build(code.default, code.default_log)?);
            Ok(0)
        }
        1 => {
            let symbol = *data
                .first()
                .ok_or_else(|| corrupt("a sequences section header is cut short"))?;
            if usize::from(symbol) > code.max_symbol {
                return Err(corrupt("a sequence code is out of range"));
            }
            *slot = Some(FseTable {
                log: 0,
                entries: vec![FseEntry {
                    symbol,
                    bits: 0,
                    base: 0,
                }],
            });
            Ok(1)
        }
        2 => {
            let (table, used) = FseTable::read(data, code.max_symbol, code.max_log)?;
            *slot = Some(table);
            Ok(used)
        }
        _ => {
            if slot.is_none() {
                return Err(corrupt("a block repeats a table no earlier block defined"));
            }
            Ok(0)
        }
    }
}

/// The offset a sequence's offset value names: a new offset, or one of the three repeated
/// offsets, which then move to the front of the list.
fn resolve_offset(
    repeated: &mut [usize; 3],
    value: u64,
    literal_length: usize,
) -> io::Result<usize> {
    if value > 3 {
        let offset = usize::try_from(value - 3)
            .map_err(|_| corrupt("an offset is larger than this computer can address"))?;
        *repeated = [offset, repeated[0], repeated[1]];
        return Ok(offset);
    }
    // With no literals before the match, the repeated offsets are shifted by one.
    let index = value as usize - 1 + usize::from(literal_length == 0);
    if index == 0 {
        return Ok(repeated[0]);
    }
    let offset = if index == 3 {
        repeated[0]
            .checked_sub(1)
            .filter(|offset| *offset > 0)
            .ok_or_else(|| corrupt("a repeated offset is zero"))?
    } else {
        repeated[index]
    };
    *repeated = if index == 1 {
        [offset, repeated[0], repeated[2]]
    } else {
        [offset, repeated[0], repeated[1]]
    };
    Ok(offset)
}

/// Appends `length` bytes copied from `offset` bytes back; the copy may overlap what it writes.
fn copy_match(
    output: &mut Vec<u8>,
    offset: usize,
    length: usize,
    window_size: usize,
) -> io::Result<()> {
    if offset == 0 || offset > output.len() || offset > window_size {
        return Err(corrupt("a match reaches outside the window"));
    }
    let start = output.len() - offset;
    if length <= offset {
        output.extend_from_within(start..start + length);
        return Ok(());
    }
    // The bytes from `start` repeat with period `offset`, so each copy can take everything
    // written since `start`, doubling the run.
    let mut left = length;
    while left > 0 {
        let run = (output.len() - start).min(left);
        output.extend_from_within(start..start + run);
        left -= run;
    }
    Ok(())
}

// FSE tables

#[derive(Clone, Copy, Default)]
struct FseEntry {
    symbol: u8,
    /// Bits read for the next state.
    bits: u8,
    /// The next state, before those bits are added.
    base: u16,
}

/// A finite-state-entropy decoding table: one entry for each of the 2^log states.
struct FseTable {
    log: u32,
    entries: Vec<FseEntry>,
}

impl FseTable {
    /// Reads a table description: the accuracy, then each symbol's normalized count. Returns
    /// the table and the description's length.
    fn read(data: &[u8], max_symbol: usize, max_log: u32) -> io::Result<(Self, usize)> {
        let mut bits = ForwardBits { data, position: 0 };
        let log = bits.read(4) + 5;
        if log > max_log {
            return Err(corrupt("an FSE table is more accurate than allowed"));
        }
        let mut counts = Vec::new();
        let mut remaining = (1i32 << log) + 1;
        let mut threshold = 1i32 << log;
        let mut width = log + 1;
        while remaining > 1 {
            if counts.len() > max_symbol {
                return Err(corrupt("an FSE table has too many symbols"));
            }
            let max = 2 * threshold - 1 - remaining;
            let small = bits.peek(width - 1) as i32;
            let value = if small < max {
                bits.consume(width - 1);
                small
            } else {
                let full = bits.peek(width) as i32;
                bits.consume(width);
                if full >= threshold {
                    full - max
                } else {
                    full
                }
            };
            // A value of 0 stands for a count of "less than one", which takes one state.
            let count = value - 1;
            remaining -= count.abs();
            if remaining < 1 {
                return Err(corrupt("an FSE table's counts overfill it"));
            }
            counts.push(count);
            if count == 0 {
                loop {
                    let zeros = bits.read(2);
                    counts.extend(std::iter::repeat_n(0, zeros as usize));
                    if zeros < 3 {
                        break;
                    }
                }
            }
            while remaining < threshold {
                width -= 1;
                threshold >>= 1;
            }
        }
        let used = bits.position.div_ceil(8);
        if remaining != 1 || counts.len() > max_symbol + 1 || used > data.len() {
            return Err(corrupt("an FSE table description is invalid"));
        }
        Ok((Self::build(&counts, log)?, used))
    }

    /// Spreads the symbols over the states and gives each state its successor.
    fn build(counts: &[i32], log: u32) -> io::Result<Self> {
        let size = 1usize << log;
        let mut symbols = vec![0u8; size];
        let mut next = vec![0u32; counts.len()];
        // Symbols of "less than one" take the last states, one each.
        let mut high = size as isize - 1;
        for (symbol, &count) in counts.iter().enumerate() {
            if count == -1 {
                if high < 0 {
                    return Err(corrupt("an FSE table is overfull"));
                }
                symbols[high as usize] = symbol as u8;
                high -= 1;
                next[symbol] = 1;
            }
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mask = size - 1;
        let mut position = 0usize;
        for (symbol, &count) in counts.iter().enumerate() {
            if count <= 0 {
                continue;
            }
            next[symbol] = count as u32;
            for _ in 0..count {
                symbols[position] = symbol as u8;
                position = (position + step) & mask;
                while position as isize > high {
                    position = (position + step) & mask;
                }
            }
        }
        if position != 0 {
            return Err(corrupt("an FSE table's counts do not fill it"));
        }
        let entries = symbols
            .iter()
            .map(|&symbol| {
                let state = next[usize::from(symbol)];
                next[usize::from(symbol)] += 1;
                let bits = log - (31 - state.leading_zeros());
                FseEntry {
                    symbol,
                    bits: bits as u8,
                    base: ((state << bits) as usize - size) as u16,
                }
            })
            .collect();
        Ok(Self { log, entries })
    }
}

// Bit readers

/// Bits read from the first byte on, lowest bit first, as table descriptions are written.
struct ForwardBits<'a> {
    data: &'a [u8],
    position: usize,
}

impl ForwardBits<'_> {
    /// The next `count` bits (at most 32); bits past the data read as zeros.
    fn peek(&self, count: u32) -> u32 {
        let word = load_le_u64(self.data, self.position / 8) >> (self.position % 8);
        (word & ((1u64 << count) - 1)) as u32
    }

    fn consume(&mut self, count: u32) {
        self.position += count as usize;
    }

    fn read(&mut self, count: u32) -> u32 {
        let value = self.peek(count);
        self.consume(count);
        value
    }
}

/// Bits read from the end of a stream back to its start, as entropy-coded streams are written:
/// the last byte's highest set bit marks where the bits begin.
struct BackwardBits<'a> {
    data: &'a [u8],
    /// How many bits are left to read.
    position: isize,
}

impl<'a> BackwardBits<'a> {
    fn new(data: &'a [u8]) -> io::Result<Self> {
        let last = *data
            .last()
            .ok_or_else(|| corrupt("an entropy-coded stream is empty"))?;
        if last == 0 {
            return Err(corrupt("an entropy-coded stream has no start marker"));
        }
        Ok(Self {
            data,
            position: data.len() as isize * 8 - 1 - last.leading_zeros() as isize,
        })
    }

    /// The next `count` bits (at most 32) without reading them; bits before the start of the
    /// stream read as zeros.
    fn peek(&self, count: u32) -> u64 {
        if count == 0 {
            return 0;
        }
        let start = self.position - count as isize;
        if start >= 0 {
            let start = start as usize;
            let word = load_le_u64(self.data, start / 8) >> (start % 8);
            return word & ((1u64 << count) - 1);
        }
        if self.position <= 0 {
            return 0;
        }
        let available = self.position as u32;
        (load_le_u64(self.data, 0) & ((1u64 << available) - 1)) << (count - available)
    }

    fn consume(&mut self, count: u32) {
        self.position -= count as isize;
    }

    fn read(&mut self, count: u32) -> u64 {
        let value = self.peek(count);
        self.consume(count);
        value
    }

    /// Every bit is read, and no more.
    fn finished(&self) -> bool {
        self.position == 0
    }

    /// More bits were read than the stream holds.
    fn overflowed(&self) -> bool {
        self.position < 0
    }
}

fn load_le_u64(data: &[u8], start: usize) -> u64 {
    match data.get(start..start + 8) {
        Some(bytes) => u64::from_le_bytes(bytes.try_into().expect("eight bytes")),
        None => {
            let mut bytes = [0u8; 8];
            if let Some(tail) = data.get(start..) {
                bytes[..tail.len()].copy_from_slice(tail);
            }
            u64::from_le_bytes(bytes)
        }
    }
}

// Content checksum

const PRIME_1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME_2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME_3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME_4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME_5: u64 = 0x27D4_EB2F_1656_67C5;

/// XXH64 with seed 0, the hash a frame's content checksum is the low 32 bits of.
struct Xxh64 {
    total: u64,
    lanes: [u64; 4],
    buffer: [u8; 32],
    buffered: usize,
}

impl Default for Xxh64 {
    fn default() -> Self {
        Self {
            total: 0,
            lanes: [
                PRIME_1.wrapping_add(PRIME_2),
                PRIME_2,
                0,
                0u64.wrapping_sub(PRIME_1),
            ],
            buffer: [0; 32],
            buffered: 0,
        }
    }
}

impl Xxh64 {
    fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        if self.buffered > 0 {
            let take = (32 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered < 32 {
                return;
            }
            let stripe = self.buffer;
            self.stripe(&stripe);
            self.buffered = 0;
        }
        let (stripes, rest) = data.as_chunks::<32>();
        for stripe in stripes {
            self.stripe(stripe);
        }
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.buffered = rest.len();
    }

    fn stripe(&mut self, stripe: &[u8; 32]) {
        let (words, _) = stripe.as_chunks::<8>();
        for (lane, word) in self.lanes.iter_mut().zip(words) {
            *lane = xxh64_round(*lane, u64::from_le_bytes(*word));
        }
    }

    fn digest(&self) -> u64 {
        let mut hash = if self.total >= 32 {
            let [a, b, c, d] = self.lanes;
            let mut hash = a
                .rotate_left(1)
                .wrapping_add(b.rotate_left(7))
                .wrapping_add(c.rotate_left(12))
                .wrapping_add(d.rotate_left(18));
            for lane in self.lanes {
                hash = (hash ^ xxh64_round(0, lane))
                    .wrapping_mul(PRIME_1)
                    .wrapping_add(PRIME_4);
            }
            hash
        } else {
            PRIME_5
        };
        hash = hash.wrapping_add(self.total);
        let mut rest = &self.buffer[..self.buffered];
        while rest.len() >= 8 {
            let lane = u64::from_le_bytes(rest[..8].try_into().expect("8 bytes"));
            hash ^= xxh64_round(0, lane);
            hash = hash
                .rotate_left(27)
                .wrapping_mul(PRIME_1)
                .wrapping_add(PRIME_4);
            rest = &rest[8..];
        }
        if rest.len() >= 4 {
            let word = u32::from_le_bytes(rest[..4].try_into().expect("4 bytes"));
            hash ^= u64::from(word).wrapping_mul(PRIME_1);
            hash = hash
                .rotate_left(23)
                .wrapping_mul(PRIME_2)
                .wrapping_add(PRIME_3);
            rest = &rest[4..];
        }
        for &byte in rest {
            hash ^= u64::from(byte).wrapping_mul(PRIME_5);
            hash = hash.rotate_left(11).wrapping_mul(PRIME_1);
        }
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(PRIME_2);
        hash ^= hash >> 29;
        hash = hash.wrapping_mul(PRIME_3);
        hash ^ (hash >> 32)
    }
}

fn xxh64_round(lane: u64, input: u64) -> u64 {
    lane.wrapping_add(input.wrapping_mul(PRIME_2))
        .rotate_left(31)
        .wrapping_mul(PRIME_1)
}

// Input

fn corrupt(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {message}"))
}

fn read_exact<R: Read>(input: &mut R, buffer: &mut [u8]) -> io::Result<()> {
    input.read_exact(buffer).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            corrupt("the data ends inside a frame")
        } else {
            error
        }
    })
}

/// A little-endian number of `count` bytes (at most 8).
fn read_le<R: Read>(input: &mut R, count: usize) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    read_exact(input, &mut bytes[..count])?;
    Ok(u64::from_le_bytes(bytes))
}

/// Fills `buffer` as far as the input goes.
fn read_up_to<R: Read>(input: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match input.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::{Decoder, Xxh64, FRAME_MAGIC};
    use std::io::{self, Cursor, Read};

    // Streams written by the reference implementation; `fixtures/zstd/generate.py` makes them.
    const TEXT_3: &[u8] = include_bytes!("../fixtures/zstd/text-3.zst");
    const TEXT_19: &[u8] = include_bytes!("../fixtures/zstd/text-19.zst");
    const TEXT_FAST: &[u8] = include_bytes!("../fixtures/zstd/text-fast.zst");
    const TEXT_STREAMED: &[u8] = include_bytes!("../fixtures/zstd/text-streamed.zst");
    const FRAMES: &[u8] = include_bytes!("../fixtures/zstd/frames.zst");
    const RUN: &[u8] = include_bytes!("../fixtures/zstd/run.zst");
    const NOISE: &[u8] = include_bytes!("../fixtures/zstd/noise.zst");
    const EMPTY: &[u8] = include_bytes!("../fixtures/zstd/empty.zst");
    const WINDOW: &[u8] = include_bytes!("../fixtures/zstd/window.zst");

    fn decode(data: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        Decoder::new(data).read_to_end(&mut out)?;
        Ok(out)
    }

    /// A single-segment frame of `content_size` bytes made of `blocks`, each a block type and
    /// its bytes.
    fn frame(content_size: u32, blocks: &[(u32, &[u8])]) -> Vec<u8> {
        let mut out = FRAME_MAGIC.to_le_bytes().to_vec();
        out.push(0xA0);
        out.extend(content_size.to_le_bytes());
        for (index, (kind, bytes)) in blocks.iter().enumerate() {
            let last = u32::from(index + 1 == blocks.len());
            let header = last | kind << 1 | (bytes.len() as u32) << 3;
            out.extend(&header.to_le_bytes()[..3]);
            out.extend_from_slice(bytes);
        }
        out
    }

    #[test]
    fn the_checksum_is_xxh64() {
        assert_eq!(Xxh64::default().digest(), 0xEF46_DB37_51D8_E999);
    }

    #[test]
    fn every_level_and_frame_shape_gives_the_same_text() {
        let text = decode(TEXT_3).expect("level 3");
        assert_eq!(text.len(), 301_626);
        assert!(text.starts_with(br#"{"timestamp":"2026-10-18T00:00:00."#));
        assert_eq!(text.iter().filter(|byte| **byte == b'\n').count(), 900);
        assert!(String::from_utf8(text.clone())
            .expect("UTF-8")
            .contains("圧縮"));
        assert_eq!(decode(TEXT_19).expect("level 19"), text);
        assert_eq!(decode(TEXT_STREAMED).expect("streamed"), text);
        assert_eq!(decode(TEXT_FAST).expect("fast"), text[..150_000]);
    }

    #[test]
    fn frames_follow_one_another_past_skippable_frames() {
        let out = decode(FRAMES).expect("frames");
        assert_eq!(out.len(), 32_980);
        let (first, second) = out.split_at(out.len() / 2);
        assert_eq!(first, second);
    }

    #[test]
    fn runs_noise_and_nothing() {
        assert_eq!(decode(RUN).expect("run"), vec![b'x'; 300_000]);
        assert_eq!(decode(NOISE).expect("noise").len(), 3_000);
        assert!(decode(EMPTY).expect("empty").is_empty());
    }

    #[test]
    fn matches_stay_right_while_old_output_is_dropped() {
        // A 128 KiB window over 2.5 MB: the decoder drops output it no longer needs, and the
        // frame's checksum confirms every byte.
        let mut decoder = Decoder::new(WINDOW);
        let mut out = Vec::new();
        decoder.read_to_end(&mut out).expect("window");
        assert_eq!(out.len(), 2_500_000);
        assert!(decoder.output.len() < 2 * 1024 * 1024);
    }

    #[test]
    fn literals_from_one_byte_and_tables_of_one_symbol() {
        // RLE literals, 32,600 sequences (the three-byte count), and single-symbol tables:
        // each sequence takes one literal and repeats the byte before it three times.
        let mut block = (1u32 | 3 << 2 | 32_600 << 4).to_le_bytes()[..3].to_vec();
        block.push(b'z');
        block.extend([255, 88, 0, 0x54, 1, 0, 0, 0x01]);
        let out = decode(&frame(130_400, &[(2, &block)])).expect("sequences");
        assert_eq!(out, vec![b'z'; 130_400]);
    }

    #[test]
    fn a_huffman_table_given_weight_by_weight() {
        // Weights for symbols 0..=97 written directly, 'a' (97) at weight 1, so 'b' (98) is
        // implied at weight 1 too; one stream holds "abba" as the bits 0110.
        let mut block = (2u32 | 4 << 4 | 51 << 14).to_le_bytes()[..3].to_vec();
        block.push(127 + 98);
        let mut weights = [0u8; 49];
        weights[48] = 0x01;
        block.extend(weights);
        block.extend([0x16, 0]);
        assert_eq!(decode(&frame(4, &[(2, &block)])).expect("abba"), b"abba");
    }

    #[test]
    fn single_symbol_tables_with_new_offsets() {
        // Raw literals "ab"; two sequences of one literal and a three-byte match, at offset 1
        // and then 2: "a" + "aaa", "b" + "aba".
        let block = [16, b'a', b'b', 2, 0x54, 1, 2, 0, 0x11];
        assert_eq!(
            decode(&frame(8, &[(2, &block)])).expect("offsets"),
            b"aaaababa"
        );
    }

    #[test]
    fn a_frame_that_needs_a_dictionary_is_refused() {
        let mut data = FRAME_MAGIC.to_le_bytes().to_vec();
        data.extend([0x21, 7, 1, 1, 0, 0, b'a']);
        let error = decode(&data).expect_err("dictionary");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("dictionary"), "{error}");
    }

    #[test]
    fn damaged_or_foreign_data_is_an_error_not_other_bytes() {
        for position in [9, 100, 4_000, 20_000, 40_000, TEXT_3.len() - 2] {
            let mut damaged = TEXT_3.to_vec();
            damaged[position] ^= 0xFF;
            let error = decode(&damaged).expect_err("damaged");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "byte {position}");
        }
        let error = decode(&TEXT_3[..TEXT_3.len() / 2]).expect_err("cut short");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = decode(b"{\"type\":\"session_meta\"}\n").expect_err("JSONL");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn small_reads_skips_and_rewinds_give_the_same_bytes() {
        let text = decode(TEXT_19).expect("level 19");
        let mut decoder = Decoder::new(Cursor::new(TEXT_19));
        let mut piecewise = Vec::new();
        let mut piece = [0u8; 7];
        loop {
            let read = decoder.read(&mut piece).expect("read");
            if read == 0 {
                break;
            }
            piecewise.extend_from_slice(&piece[..read]);
        }
        assert_eq!(piecewise, text);

        decoder.rewind().expect("rewind");
        assert_eq!(decoder.skip(200_000).expect("skip"), 200_000);
        let mut part = vec![0u8; 1_000];
        decoder.read_exact(&mut part).expect("read after skip");
        assert_eq!(part, text[200_000..201_000]);
        assert_eq!(decoder.skip(u64::MAX).expect("skip to end"), 100_626);

        decoder.rewind().expect("rewind again");
        let mut again = Vec::new();
        decoder.read_to_end(&mut again).expect("read again");
        assert_eq!(again, text);
    }

    #[test]
    fn the_declared_size_is_read_from_the_header() {
        assert_eq!(
            super::declared_content_size(&mut &TEXT_3[..]).expect("header"),
            Some(301_626)
        );
        assert_eq!(
            super::declared_content_size(&mut &TEXT_STREAMED[..]).expect("header"),
            None
        );
    }
}
