use std::{marker::PhantomData, ops::Range};

use embedded_storage_async::nor_flash::ErrorType;
use minicbor::{
    Decode, Encode,
    encode::write::{Cursor, EndOfSlice},
};
use sequential_storage::{
    Error,
    cache::{KeyCacheImpl, NoCache},
    map::{self, Key, MapItemIter, Value},
    mock_flash::{MockFlashBase, WriteCountCheck},
};

struct WrapBuilder<K: Key, const PAGES: usize, const BYTES_PER_WORD: usize, const PAGE_WORDS: usize>
{
    _pd: PhantomData<fn() -> K>,
    wcc: WriteCountCheck,
    bus: Option<u32>,
    ac: bool,
}

impl WrapBuilder<u32, 0, 0, 0> {
    const fn new() -> Self {
        Self {
            _pd: PhantomData,
            wcc: WriteCountCheck::Twice,
            bus: None,
            ac: true,
        }
    }
}

#[allow(dead_code)]
impl<K, const P: usize, const BPW: usize, const PW: usize> WrapBuilder<K, P, BPW, PW>
where
    K: Key,
{
    const fn with_pages<const NP: usize>(self) -> WrapBuilder<K, NP, BPW, PW> {
        WrapBuilder {
            _pd: PhantomData,
            wcc: self.wcc,
            bus: self.bus,
            ac: self.ac,
        }
    }

    const fn with_bytes_per_word<const NBPW: usize>(self) -> WrapBuilder<K, P, NBPW, PW> {
        WrapBuilder {
            _pd: PhantomData,
            wcc: self.wcc,
            bus: self.bus,
            ac: self.ac,
        }
    }

    const fn with_page_words<const NPW: usize>(self) -> WrapBuilder<K, P, BPW, NPW> {
        WrapBuilder {
            _pd: PhantomData,
            wcc: self.wcc,
            bus: self.bus,
            ac: self.ac,
        }
    }

    const fn with_key<NK: Key>(self) -> WrapBuilder<NK, P, BPW, PW> {
        WrapBuilder {
            _pd: PhantomData,
            wcc: self.wcc,
            bus: self.bus,
            ac: self.ac,
        }
    }

    const fn with_write_count_check(self, write_count_check: WriteCountCheck) -> Self {
        WrapBuilder {
            _pd: PhantomData,
            wcc: write_count_check,
            bus: self.bus,
            ac: self.ac,
        }
    }

    const fn with_bytes_until_shutoff(self, bytes_until_shutoff: u32) -> Self {
        WrapBuilder {
            _pd: PhantomData,
            wcc: self.wcc,
            bus: Some(bytes_until_shutoff),
            ac: self.ac,
        }
    }

    const fn without_shutoff(self) -> Self {
        WrapBuilder {
            _pd: PhantomData,
            wcc: self.wcc,
            bus: None,
            ac: self.ac,
        }
    }

    const fn with_alignment_check(self, alignment_check: bool) -> Self {
        WrapBuilder {
            _pd: PhantomData,
            wcc: self.wcc,
            bus: self.bus,
            ac: alignment_check,
        }
    }

    fn build<C: KeyCacheImpl<K>>(self, cache: C, range: Range<u32>) -> Wrap<C, K, P, BPW, PW> {
        // todo: assert some stuff is sane
        Wrap {
            flash: MockFlashBase::<P, BPW, PW>::new(self.wcc, self.bus, self.ac),
            range,
            cache,
            _pd: PhantomData,
        }
    }
}

struct Wrap<
    C: KeyCacheImpl<K>,
    K: Key,
    const PAGES: usize = 0,
    const BYTES_PER_WORD: usize = 0,
    const PAGE_WORDS: usize = 0,
> {
    flash: MockFlashBase<PAGES, BYTES_PER_WORD, PAGE_WORDS>,
    range: Range<u32>,
    cache: C,
    _pd: PhantomData<fn() -> K>,
}

#[allow(dead_code)]
impl<C, K, const P: usize, const BPW: usize, const PW: usize> Wrap<C, K, P, BPW, PW>
where
    K: Key,
    C: KeyCacheImpl<K>,
{
    async fn fetch_item<'d, V: Value<'d>>(
        &mut self,
        k: &K,
        buf: &'d mut [u8],
    ) -> Result<Option<V>, Error<<MockFlashBase<P, BPW, PW> as ErrorType>::Error>> {
        let Self {
            flash,
            range,
            cache,
            _pd,
        } = self;
        map::fetch_item(flash, range.clone(), cache, buf, k).await
    }

    async fn store_item<'d, V: Value<'d>>(
        &mut self,
        k: &K,
        v: &V,
        buf: &mut [u8],
    ) -> Result<(), Error<<MockFlashBase<P, BPW, PW> as ErrorType>::Error>> {
        let Self {
            flash,
            range,
            cache,
            _pd,
        } = self;
        map::store_item(flash, range.clone(), cache, buf, k, v).await
    }

    async fn fetch_all_items(
        &mut self,
        buf: &mut [u8],
    ) -> Result<
        MapItemIter<'_, '_, MockFlashBase<P, BPW, PW>, C>,
        Error<<MockFlashBase<P, BPW, PW> as ErrorType>::Error>,
    > {
        let Self {
            flash,
            range,
            cache,
            _pd,
        } = self;
        map::fetch_all_items(flash, range.clone(), cache, buf).await
    }
}

impl<C, K, const P: usize, const BPW: usize, const PW: usize> Wrap<C, K, P, BPW, PW>
where
    K: Key,
    C: KeyCacheImpl<K>,
{
    async fn fetch_datum<'a, T: Decode<()>>(
        &mut self,
        k: &str,
    )
}

#[tokio::main]
async fn main() {
    inner_main().await;
}

async fn inner_main() {
    const BYTES_PER_WORD: usize = 4;
    const PAGE_WORDS: usize = 256;
    const PAGES: usize = 4;

    let mut buf = [0xFFu8; 4096];
    let mut flash = WrapBuilder::new()
        .with_pages::<PAGES>()
        .with_bytes_per_word::<BYTES_PER_WORD>()
        .with_page_words::<PAGE_WORDS>()
        .with_key::<u64>()
        .with_write_count_check(WriteCountCheck::Twice)
        .with_alignment_check(true)
        .without_shutoff()
        .build(
            NoCache::new(),
            0..((BYTES_PER_WORD * PAGE_WORDS * PAGES) as u32),
        );

    let config = ConfigV1 {
        brightness: 1.0,
        volume: 23.0,
    };

    let mut cursor = Cursor::new(buf.as_mut());
    minicbor::encode(&config, &mut cursor).unwrap();
    let used = cursor.position();
    println!("{used}");
    println!("{}", hexdump(&buf));

    let buffy = &buf[..used];
    let v1 = minicbor::decode::<ConfigV1>(buffy).unwrap();

    // First revision with only two fields
    println!("v1: {v1:?}");
    let v1_1a = minicbor::decode::<ConfigV1_1a>(buffy);
    // Second revision with three fields - decoding will fail!
    println!("v1_1a: {v1_1a:?}");
    let v1_1b = minicbor::decode::<ConfigV1_1b>(buffy).unwrap();
    // Second revision with three fields, but the new one is optional - will work!
    println!("v1_1b: {v1_1b:?}");
    // Third revision with three fields, the new one declares `default`, so default value
    // will be used.
    let v1_1c = minicbor::decode::<ConfigV1_1c>(buffy).unwrap();
    println!("v1_1c: {v1_1c:?}");

    buf.iter_mut().for_each(|b| *b = 0xFF);
    let param = Outer {
        param: Parameter { name: "billy" },
    };
    let mut cursor = Cursor::new(buf.as_mut());
    minicbor::encode(&param, &mut cursor).unwrap();
    let used = cursor.position();
    println!("{used}");
    println!("{}", hexdump(&buf));
    let buffy = &buf[..used];
    let p = minicbor::decode::<Outer<'_>>(buffy).unwrap();
    println!("{p:?}");
}

const CURRENT_HEADER: Header = Header {
    magic: 0x07,
    major: 0,
    minor: 1,
    trivial: 0,
};
struct Header {
    magic: u8,
    major: u8,
    minor: u8,
    trivial: u8,
}

#[derive(Clone, PartialEq, Eq)]
struct Path<'a> {
    path: &'a [u8],
}

impl<'a> Path<'a> {
    pub const fn from_str(s: &'a str) -> Self {
        Self::try_from_str(s).expect("Expected a string less than 255 bytes long!")
    }

    pub const fn from_slice(buf: &'a [u8]) -> Self {
        Self::try_from_slice(buf).expect("Expected a string less than 255 bytes long!")
    }

    pub const fn try_from_str(s: &'a str) -> Option<Self> {
        let buf = s.as_bytes();
        if buf.len() > (u8::MAX as usize) {
            return None;
        }
        Some(Self { path: buf })
    }

    pub const fn try_from_slice(buf: &'a [u8]) -> Option<Self> {
        if buf.len() > (u8::MAX as usize) {
            return None;
        }
        Some(Self { path: buf })
    }
}

impl<'a> Key for Path<'a> {
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, map::SerializationError> {
        let len = 1 + self.path.len();
        if buffer.len() < len {
            return Err(map::SerializationError::BufferTooSmall);
        }
        let to_use = &mut buffer[..len];
        to_use[0] = self.path.len() as u8;
        to_use[1..].copy_from_slice(self.path);
        Ok(len)
    }

    fn deserialize_from(buffer: &[u8]) -> Result<(Self, usize), map::SerializationError> {
        let Some((len, rest)) = buffer.split_first() else {
            return Err(map::SerializationError::BufferTooSmall);
        };
        if rest.len() != (*len as usize) {
            return Err(map::SerializationError::InvalidFormat);
        }
        Ok((Self { path: rest }, buffer.len()))
    }
}

struct Datum<T> {
    hdr: Header,
    body: T,
}

impl<'a, T> Value<'a> for Datum<T>
where
    T: Encode<()>,
    T: Decode<'a, ()>,
{
    fn serialize_into(&self, buffer: &mut [u8]) -> Result<usize, map::SerializationError> {
        // first serialize the header
        let data = [
            CURRENT_HEADER.magic,
            CURRENT_HEADER.major,
            CURRENT_HEADER.minor,
            CURRENT_HEADER.trivial,
        ];
        let Some((now, later)) = buffer.split_at_mut_checked(data.len()) else {
            return Err(map::SerializationError::BufferTooSmall);
        };
        now.copy_from_slice(&data);

        // Then serialize the CBOR encoded data
        let mut cursor = Cursor::new(later);
        let res: Result<(), minicbor::encode::Error<EndOfSlice>> =
            minicbor::encode(&self.body, &mut cursor);
        match res {
            Ok(()) => {}
            Err(_e) => {
                // We know this was an "end of slice" error
                return Err(map::SerializationError::BufferTooSmall);
            }
        }
        let used = now.len() + cursor.position();
        Ok(used)
    }

    fn deserialize_from(buffer: &'a [u8]) -> Result<Self, map::SerializationError>
    where
        Self: Sized,
    {
        // First decode and check the header
        let Some(([magic, major, minor, trivial], body)) = buffer.split_at_checked(4) else {
            return Err(map::SerializationError::InvalidFormat);
        };
        if *magic != CURRENT_HEADER.magic || *major != CURRENT_HEADER.major {
            return Err(map::SerializationError::InvalidFormat);
        }

        let res: Result<T, minicbor::decode::Error> = minicbor::decode(body);
        match res {
            Ok(body) => Ok(Datum {
                hdr: Header {
                    magic: *magic,
                    major: *major,
                    minor: *minor,
                    trivial: *trivial,
                },
                body,
            }),
            Err(_e) => {
                // TODO: could log the reason, it would require `Display` though.
                Err(map::SerializationError::InvalidData)
            }
        }
    }
}

#[derive(Debug, Decode, Encode)]
struct ConfigV1 {
    #[n(1)]
    brightness: f32,
    #[n(2)]
    volume: f32,
}

#[derive(Debug, Decode, Encode)]
struct ConfigV1_1a {
    #[n(1)]
    brightness: f32,
    #[n(2)]
    volume: f32,
    #[n(3)]
    vibration: f32,
}

#[derive(Debug, Decode, Encode)]
struct ConfigV1_1b {
    #[n(1)]
    brightness: f32,
    #[n(2)]
    volume: f32,
    #[n(3)]
    vibration: Option<f32>,
}

#[derive(Debug, Decode, Encode)]
struct ConfigV1_1c {
    #[n(1)]
    brightness: f32,
    #[n(2)]
    volume: f32,
    #[n(3)]
    #[cbor(default)]
    vibration: f32,
}

#[derive(Debug, Decode, Encode)]
struct Parameter<'a> {
    #[n(1)]
    name: &'a str,
}

#[derive(Debug, Decode, Encode)]
struct Outer<'a> {
    #[b(1)]
    param: Parameter<'a>,
}

fn hexdump(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::new();
    let mut blanks = false;
    for (i, ch) in bytes.chunks(16).enumerate() {
        let all_blank = ch.iter().all(|b| *b == 0xFF);
        if all_blank {
            blanks = true;
            continue;
        } else if blanks {
            s.push_str("...all bytes blank...\n");
            blanks = false;
        }

        write!(&mut s, "{:08X} | ", i * 16).ok();
        for b in ch {
            write!(&mut s, "{b:02X} ").ok();
        }
        for _ in 0..(16 - ch.len()) {
            write!(&mut s, "-- ").ok();
        }
        // todo print ascii'd data?
        //
        s.push_str("|\n");
    }
    if blanks {
        s.push_str("...all bytes blank...\n");
    }
    s
}
