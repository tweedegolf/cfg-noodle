use std::{marker::PhantomData, ops::Range};

use embedded_storage_async::nor_flash::ErrorType;
use sequential_storage::{
    cache::{KeyCacheImpl, NoCache}, map::{self, Key, MapItemIter, Value}, mock_flash::{MockFlashBase, WriteCountCheck}, Error
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

    async fn fetch_all_items(&mut self, buf: &mut [u8]) -> Result<
        MapItemIter<'_, '_, MockFlashBase<P, BPW, PW>, C>,
        Error<<MockFlashBase<P, BPW, PW> as ErrorType>::Error>,
    >
    {
        let Self {
            flash,
            range,
            cache,
            _pd,
        } = self;
        map::fetch_all_items(flash, range.clone(), cache, buf).await
    }
}

#[tokio::main]
async fn main() {
    inner_main().await;
}

async fn inner_main() {
    const BYTES_PER_WORD: usize = 4;
    const PAGE_WORDS: usize = 256;
    const PAGES: usize = 4;

    let mut buf = [0u8; 4096];
    let mut flash = WrapBuilder::new()
        .with_pages::<PAGES>()
        .with_bytes_per_word::<BYTES_PER_WORD>()
        .with_page_words::<PAGE_WORDS>()
        .with_key::<u64>()
        .with_write_count_check(WriteCountCheck::Twice)
        .with_alignment_check(true)
        .without_shutoff()
        .build(NoCache::new(), 0..((BYTES_PER_WORD * PAGE_WORDS * PAGES) as u32));

    println!("Read empty");
    let res = flash.fetch_item::<i64>(&123, &mut buf).await;
    let res = res.unwrap();
    assert!(res.is_none());

    println!("Store");
    flash.store_item(&123, &456i64, &mut buf).await.unwrap();

    println!("Retrieve");
    let res = flash.fetch_item::<i64>(&123, &mut buf).await;
    let res = res.unwrap();
    assert_eq!(Some(456), res);

    println!("Success!");

    println!("{}", hexdump(flash.flash.as_bytes()));

    // How does overwriting work?
    for i in 0..240 {
        println!("{i}");
        flash.store_item(&i, &((10 * i) as i64), &mut buf).await.unwrap();
        println!("{}", hexdump(flash.flash.as_bytes()));
    }
    // for i in 0..24 {
    //     if (i % 4) != 0 {
    //         continue;
    //     }
    //     flash.store_item(&i, &((20 * i) as i64), &mut buf).await.unwrap();
    // }
    // let mut it = flash.fetch_all_items(&mut buf).await.unwrap();
    // loop {
    //     let next = it.next::<u64, i64>(&mut buf).await.unwrap();
    //     let Some((k, v)) = next else {
    //         break;
    //     };
    //     println!("k:{k}, v:{v}");
    // }
    // println!("---");
    // for i in 0..24 {
    //     let item = flash.fetch_item::<u64>(&i, &mut buf).await.unwrap().unwrap();
    //     println!("k:{i}, v: {item}");
    // }
    // println!("---");
    // let mut it = flash.fetch_all_items(&mut buf).await.unwrap();
    // loop {
    //     let next = it.next::<u64, &[u8]>(&mut buf).await.unwrap();
    //     let Some((k, v)) = next else {
    //         break;
    //     };
    //     println!("k:{k}, v:{v:?}");
    // }
    // println!("---");
    // for i in 0..24 {
    //     let item = flash.fetch_item::<&[u8]>(&i, &mut buf).await.unwrap().unwrap();
    //     println!("k:{i}, v: {item:?}");
    // }
    // println!("---");
    // println!("{}", hexdump(flash.flash.as_bytes()));

    // for n in 0..256 {
    //     println!("Overwrite {n}...");
    //     for i in 0..24 {
    //         flash.store_item(&i, &(n as i64), &mut buf).await.unwrap();
    //     }
    // }
    // println!("{}", hexdump(flash.flash.as_bytes()));
    // println!("---");
    // let mut it = flash.fetch_all_items(&mut buf).await.unwrap();
    // let mut ct = 0;
    // loop {
    //     let next = it.next::<u64, i64>(&mut buf).await.unwrap();
    //     let Some((k, v)) = next else {
    //         break;
    //     };
    //     ct += 1;
    //     println!("k:{k}, v:{v}");
    // }
    // println!("ct: {ct}");
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
