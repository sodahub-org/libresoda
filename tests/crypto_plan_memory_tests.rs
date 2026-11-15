//! 票 #49：计划阶段不按声明的样本数展开，峰值分配不随样本数放大。
//!
//! 单独放一个测试文件：计数分配器是全局的，同一个二进制里跑别的用例会互相干扰。

use libresoda::soda::crypto::{decrypt_audio_with_key, decrypt_stream};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = System.alloc(layout);
        if !ptr.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const KEY: [u8; 16] = [0u8; 16];

fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

/// 一条轨的 `stbl`：定长 `stsz`（`count` 个 `sample_size` 字节）+ 单条 `stsc`/`stco`，
/// 带 `senc` 时再加一条解密记录。
fn track_stbl(sample_size: u32, count: u32, chunk_offset: u32, with_senc: bool) -> Vec<u8> {
    let mut stsz = vec![0u8; 12];
    stsz[4..8].copy_from_slice(&sample_size.to_be_bytes());
    stsz[8..12].copy_from_slice(&count.to_be_bytes());

    let mut stsc = vec![0u8; 4 + 4 + 12];
    stsc[4..8].copy_from_slice(&1u32.to_be_bytes());
    stsc[8..12].copy_from_slice(&1u32.to_be_bytes());
    stsc[12..16].copy_from_slice(&count.to_be_bytes());

    let mut stco = vec![0u8; 4 + 4 + 4];
    stco[4..8].copy_from_slice(&1u32.to_be_bytes());
    stco[8..12].copy_from_slice(&chunk_offset.to_be_bytes());

    let mut payload = Vec::new();
    payload.extend_from_slice(&mp4_box(b"stsz", &stsz));
    payload.extend_from_slice(&mp4_box(b"stsc", &stsc));
    payload.extend_from_slice(&mp4_box(b"stco", &stco));
    if with_senc {
        let mut senc = vec![0u8; 8 + 8];
        senc[4..8].copy_from_slice(&1u32.to_be_bytes());
        senc[8..16].copy_from_slice(&[1u8; 8]);
        payload.extend_from_slice(&mp4_box(b"senc", &senc));
    }
    mp4_box(b"stbl", &payload)
}

/// 畸形构造：明文轨声明 `sample_count` 个 1 字节样本，实际 `mdat` 只有 16 字节；
/// 另一条小音频轨带上 `senc`，让计划能走到样本位置展开这一步。
///
/// 单个 chunk 装下全部样本，表本身只有几十字节——文件很小，声明很大。
fn huge_fixture(sample_count: u32) -> Vec<u8> {
    let mdat_payload_len = 16u32;
    let build = |payload_start: u32| {
        let text = mp4_box(
            b"trak",
            &mp4_box(
                b"mdia",
                &mp4_box(b"minf", &track_stbl(1, sample_count, payload_start, false)),
            ),
        );
        let audio = mp4_box(
            b"trak",
            &mp4_box(
                b"mdia",
                &mp4_box(b"minf", &track_stbl(16, 1, payload_start + 1000, true)),
            ),
        );
        let mut moov_payload = Vec::new();
        moov_payload.extend_from_slice(&text);
        moov_payload.extend_from_slice(&audio);
        let mut file = mp4_box(b"moov", &moov_payload);
        file.extend_from_slice(&mp4_box(b"mdat", &[9u8; mdat_payload_len as usize]));
        file
    };
    // 先量出 moov 长度，再把 chunk 偏移指到 mdat 载荷起点
    let probe = build(0);
    let payload_start = (probe.len() - mdat_payload_len as usize) as u32;
    build(payload_start)
}

/// 跑一段代码，返回结果和这段时间里的峰值分配增量。
fn measure<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let value = run();
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    (value, peak)
}

/// 票 #49：小文件声明 100 万样本时，两条路径都必须不展开位置表就拒绝，
/// 峰值分配留在 KB 级，不再出现过去的几十上百 MB。
#[test]
fn huge_declared_sample_count_keeps_plan_memory_small() {
    // 卡在计划上限 1M（票 #44）以内，正好压测惰性游标。
    let file = huge_fixture(1_000_000);
    assert!(
        file.len() < 4096,
        "构造文件应该很小，实际 {} 字节",
        file.len()
    );

    let (stream, stream_peak) = measure(|| {
        let mut input = std::io::Cursor::new(file.clone());
        let mut out = Vec::new();
        decrypt_stream(&mut input, &KEY, &mut out)
    });
    assert!(stream.is_err(), "声明的样本装不进 mdat，应报错");
    assert!(
        stream_peak < 8 * 1024 * 1024,
        "流式路径峰值 {stream_peak} 字节，超过 8MiB"
    );

    let (batch, batch_peak) = measure(|| decrypt_audio_with_key(&file, &KEY));
    assert!(batch.is_err(), "声明的样本装不进 mdat，应报错");
    assert!(
        batch_peak < 8 * 1024 * 1024,
        "整块路径峰值 {batch_peak} 字节，超过 8MiB"
    );
}
