//! MD 열기(워커): 파일 읽기 → 인코딩 감지 → 파싱 → 레이아웃 단위 빌드 → 로컬 이미지 크기 조회.
//! 이미지 디코딩도 워커에서 WIC로 한다 (FR-M03: 표시 크기로 디코딩, 원본 해상도로 보관하지 않음).
//! 파일은 모두 공유 거부 없이 연다 (§6.2): 이미지는 `SHCreateStreamOnFileEx(STGM_SHARE_DENY_NONE)`.

use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use leaf_layout::content::Content;
use leaf_layout::flow::ImgSize;
use windows::Win32::Globalization::GetACP;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
use windows::core::{PCWSTR, Result};

use crate::util::wide;

/// 문서를 열 때 크기를 미리 알아 둘 이미지 수 (나머지는 보일 때 조회)
const PROBE_MAX: usize = 128;

/// 이미지 원본 위치
#[derive(Debug, Clone)]
pub enum ImgSrc {
    Local(PathBuf),
    /// 원격 이미지는 기본 차단 (FR-M14, §6.3)
    Remote,
    /// data: 등 지원하지 않는 형식
    Unsupported,
}

pub struct LoadedMd {
    pub content: Content,
    pub sizes: Vec<ImgSize>,
    pub srcs: Vec<ImgSrc>,
    pub encoding: &'static str,
    pub size: u64,
    pub read_ms: f64,
    pub parse_ms: f64,
    pub probe_ms: f64,
}

/// 워커에서 호출. `root`: 트리 루트(`/`로 시작하는 경로의 기준)
pub fn load(path: &Path, root: Option<&Path>) -> std::result::Result<LoadedMd, String> {
    let t = Instant::now();
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let read_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let (text, encoding) = leaf_fmt_markdown::decode(&bytes, unsafe { GetACP() });
    drop(bytes);
    let doc = leaf_fmt_markdown::parse(&text);
    drop(text);
    let content = Content::build(&doc);
    let parse_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let base = path.parent().unwrap_or(Path::new(""));
    let srcs: Vec<ImgSrc> = content
        .images
        .iter()
        .map(|im| match resolve(base, root, &im.src) {
            Target::Local(p, _) => ImgSrc::Local(p),
            Target::Uri(u) if is_remote(&u) => ImgSrc::Remote,
            _ => ImgSrc::Unsupported,
        })
        .collect();
    let _com = ComInit::new();
    let wic = factory().ok();
    let sizes = srcs
        .iter()
        .enumerate()
        .map(|(i, s)| match s {
            ImgSrc::Local(p) if i < PROBE_MAX => match wic.as_ref().map(|f| probe(f, p)) {
                Some(Ok((w, h))) => ImgSize::Known(w as f32, h as f32),
                _ => ImgSize::Failed,
            },
            ImgSrc::Local(_) => ImgSize::Unknown,
            _ => ImgSize::Failed,
        })
        .collect();
    let probe_ms = t.elapsed().as_secs_f64() * 1000.0;
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Ok(LoadedMd {
        content,
        sizes,
        srcs,
        encoding,
        size,
        read_ms,
        parse_ms,
        probe_ms,
    })
}

fn is_remote(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("//")
}

// ───── 링크·이미지 경로 해석 (FR-M04) ─────

#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// 같은 문서 안 `#앵커`
    Anchor(String),
    /// 로컬 파일 (+ `#앵커`)
    Local(PathBuf, Option<String>),
    /// 스킴이 있는 URI (http, mailto, …)
    Uri(String),
}

/// `%XX` 해독 (UTF-8)
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `http:`처럼 스킴이 있으면 그 이름. 드라이브 문자(`C:`)는 스킴이 아니다.
fn scheme(s: &str) -> Option<&str> {
    let i = s.find(':')?;
    let sch = &s[..i];
    let ok = sch.len() > 1
        && sch.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && sch
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    ok.then_some(sch)
}

/// `.`·`..` 정리 (파일 시스템에 묻지 않는다)
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// MD 안의 링크·이미지 주소 해석. `base`: 문서가 있는 폴더, `root`: 트리 루트.
pub fn resolve(base: &Path, root: Option<&Path>, href: &str) -> Target {
    let href = href.trim();
    if let Some(a) = href.strip_prefix('#') {
        return Target::Anchor(percent_decode(a));
    }
    if let Some(sch) = scheme(href) {
        if sch.eq_ignore_ascii_case("file") {
            let rest = href[5..].trim_start_matches('/');
            let (p, frag) = split_frag(rest);
            let p = percent_decode(p).replace('/', "\\");
            return Target::Local(normalize(Path::new(&p)), frag);
        }
        return Target::Uri(href.to_string());
    }
    if href.starts_with("//") {
        return Target::Uri(format!("https:{href}"));
    }
    let (p, frag) = split_frag(href);
    let p = p.split('?').next().unwrap_or("");
    let p = percent_decode(p).replace('/', "\\");
    let path = Path::new(&p);
    let full = if path.has_root() && path.is_absolute() {
        path.to_path_buf()
    } else if p.starts_with('\\') {
        // `/docs/a.md`: 트리 루트 기준 (GitHub의 저장소 루트처럼)
        match root {
            Some(r) => r.join(p.trim_start_matches('\\')),
            None => base.join(p.trim_start_matches('\\')),
        }
    } else {
        base.join(path)
    };
    Target::Local(normalize(&full), frag)
}

fn split_frag(s: &str) -> (&str, Option<String>) {
    match s.split_once('#') {
        Some((p, f)) => (p, (!f.is_empty()).then(|| percent_decode(f))),
        None => (s, None),
    }
}

// ───── WIC ─────

/// 워커 스레드 COM 초기화 (MTA). 이미 초기화돼 있으면 아무 일도 하지 않는다.
pub struct ComInit(bool);

impl ComInit {
    pub fn new() -> Self {
        Self(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

pub(crate) fn factory() -> Result<IWICImagingFactory> {
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }
}

pub(crate) fn open_frame(f: &IWICImagingFactory, path: &Path) -> Result<IWICBitmapFrameDecode> {
    let wp = wide(path);
    unsafe {
        let stream = SHCreateStreamOnFileEx(
            PCWSTR(wp.as_ptr()),
            (STGM_READ | STGM_SHARE_DENY_NONE).0,
            0,
            false,
            None,
        )?;
        let dec =
            f.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        // GIF는 첫 프레임 (FR-M03)
        dec.GetFrame(0)
    }
}

fn probe(f: &IWICImagingFactory, path: &Path) -> Result<(u32, u32)> {
    let frame = open_frame(f, path)?;
    let (mut w, mut h) = (0u32, 0u32);
    unsafe { frame.GetSize(&mut w, &mut h)? };
    Ok((w, h))
}

pub struct DecodedImage {
    /// 원본 크기 (px)
    pub iw: u32,
    pub ih: u32,
    /// 디코딩한 크기 (px)
    pub pw: u32,
    pub ph: u32,
    /// 미리 곱한 알파 BGRA, stride = pw × 4
    pub bgra: Vec<u8>,
}

/// 원본을 `max_w` px 폭 이하로 줄여 디코딩한다 (확대하지 않는다).
pub fn decode(path: &Path, max_w: u32) -> std::result::Result<DecodedImage, String> {
    let _com = ComInit::new();
    let r: Result<DecodedImage> = (|| unsafe {
        let f = factory()?;
        let frame = open_frame(&f, path)?;
        let (mut iw, mut ih) = (0u32, 0u32);
        frame.GetSize(&mut iw, &mut ih)?;
        if iw == 0 || ih == 0 {
            return Err(windows::core::Error::empty());
        }
        let pw = iw.min(max_w.max(1));
        let ph = ((ih as u64 * pw as u64) / iw as u64).max(1) as u32;
        let source: IWICBitmapSource = if pw < iw {
            let s = f.CreateBitmapScaler()?;
            s.Initialize(&frame, pw, ph, WICBitmapInterpolationModeHighQualityCubic)?;
            s.into()
        } else {
            frame.into()
        };
        let conv = f.CreateFormatConverter()?;
        conv.Initialize(
            &source,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )?;
        let stride = pw * 4;
        let mut bgra = vec![0u8; stride as usize * ph as usize];
        conv.CopyPixels(std::ptr::null(), stride, &mut bgra)?;
        Ok(DecodedImage {
            iw,
            ih,
            pw,
            ph,
            bgra,
        })
    })();
    r.map_err(|e| {
        if e.message().is_empty() {
            format!("이미지를 읽을 수 없습니다 ({:?})", e.code())
        } else {
            e.message()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_links() {
        let base = Path::new(r"C:\docs\notes");
        let root = Some(Path::new(r"C:\docs"));
        assert_eq!(
            resolve(base, root, "#설치-방법"),
            Target::Anchor("설치-방법".into())
        );
        assert_eq!(
            resolve(base, root, "#%EC%84%A4%EC%B9%98"),
            Target::Anchor("설치".into())
        );
        assert_eq!(
            resolve(base, root, "../a%20b.md#Sec"),
            Target::Local(PathBuf::from(r"C:\docs\a b.md"), Some("Sec".into()))
        );
        assert_eq!(
            resolve(base, root, "/img/x.png?raw=1"),
            Target::Local(PathBuf::from(r"C:\docs\img\x.png"), None)
        );
        assert_eq!(
            resolve(base, root, r"D:\abs\y.md"),
            Target::Local(PathBuf::from(r"D:\abs\y.md"), None)
        );
        assert_eq!(
            resolve(base, root, "file:///C:/t/z.txt"),
            Target::Local(PathBuf::from(r"C:\t\z.txt"), None)
        );
        assert_eq!(
            resolve(base, root, "https://x.y/#a"),
            Target::Uri("https://x.y/#a".into())
        );
        assert_eq!(
            resolve(base, root, "mailto:a@b"),
            Target::Uri("mailto:a@b".into())
        );
        assert_eq!(
            resolve(base, root, "./sub/./c.md"),
            Target::Local(PathBuf::from(r"C:\docs\notes\sub\c.md"), None)
        );
    }

    #[test]
    fn percent() {
        assert_eq!(percent_decode("a%20b%2"), "a b%2");
        assert_eq!(percent_decode("%zz%41"), "%zzA");
    }
}
