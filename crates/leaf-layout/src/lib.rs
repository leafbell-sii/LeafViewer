//! Flow IR → 블록 레이아웃 (SPEC §4.1 `leaf-layout`, §4.4). 텍스트 측정은 `Measurer` trait로 묻고,
//! 실제 구현은 leaf-win(DirectWrite)이, 테스트는 가짜 구현이 제공한다.
#![forbid(unsafe_code)]

pub mod content;
pub mod flow;

pub use content::{Content, ImageId, ParaId, ParaKind};
pub use flow::{BlockLayout, FillKind, FlowLayout, ImgSize, Item, Measured, Measurer};
