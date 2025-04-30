use std::{marker::PhantomData, ops::Range};

use embedded_storage_async::nor_flash::ErrorType;
use sequential_storage::{
    Error,
    cache::{KeyCacheImpl, NoCache},
    map::{self, Key, Value},
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
}

#[tokio::main]
async fn main() {
    println!("Hello, world!");
    let mut buf = [0u8; 4096];
    let mut flash = WrapBuilder::new()
        .with_pages::<128>()
        .with_bytes_per_word::<4>()
        .with_page_words::<1024>()
        .with_key::<u64>()
        .with_write_count_check(WriteCountCheck::Twice)
        .with_alignment_check(true)
        .without_shutoff()
        .build(NoCache::new(), 0..(64 * 4096));

    let res = flash.fetch_item::<i64>(&123, &mut buf).await;
    let res = res.unwrap();
    assert!(res.is_none());

    // let got =
}
