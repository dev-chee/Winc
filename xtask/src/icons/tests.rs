//! `icons` 的测试：提交的 Windows 图标资源与 SVG 一致，生成的 ICO 和 RES 使用同一组图像。

use super::*;

/// 逐条读出 RES 的（类型，名称，数据）。
fn resource_records(res: &[u8]) -> Vec<(u16, u16, &[u8])> {
    let long = |at: usize| u32::from_le_bytes(res[at..at + 4].try_into().unwrap());
    let word = |at: usize| u16::from_le_bytes(res[at..at + 2].try_into().unwrap());
    let mut records = Vec::new();
    let mut at = 0;
    while at < res.len() {
        let (size, header) = (long(at) as usize, long(at + 4) as usize);
        assert_eq!(header, 32);
        assert_eq!((word(at + 8), word(at + 12)), (0xffff, 0xffff));
        let data = &res[at + header..at + header + size];
        records.push((word(at + 10), word(at + 14), data));
        at = (at + header + size).next_multiple_of(4);
    }
    assert_eq!(at, res.len());
    records
}

#[test]
fn the_committed_icon_matches_the_svg() {
    let text = std::fs::read_to_string(root().join("crates/winc-app/assets/icon.svg")).unwrap();
    let picture = winc_preview::svg::load(&text, None, None).unwrap();
    let res = std::fs::read(root().join("crates/winc-app/assets/winc.res")).unwrap();
    let records = resource_records(&res);
    let sizes = [16, 32, 48, 256];
    assert_eq!(records.len(), sizes.len() + 2);
    assert_eq!(records[0], (0, 0, &[][..]));
    let (kind, id, group) = records[sizes.len() + 1];
    assert_eq!((kind, id), (14, ICON_GROUP));
    assert_eq!(group.len(), 6 + 14 * sizes.len());
    assert_eq!(group[..6], [0, 0, 1, 0, 4, 0]);
    for (index, size) in sizes.into_iter().enumerate() {
        let (kind, id, bytes) = records[index + 1];
        assert_eq!((kind, id), (3, index as u16 + 1));
        let entry = &group[6 + 14 * index..6 + 14 * (index + 1)];
        let dimension = size as u8;
        assert_eq!(entry[..8], [dimension, dimension, 0, 0, 1, 0, 32, 0]);
        assert_eq!(entry[8..12], (bytes.len() as u32).to_le_bytes());
        assert_eq!(entry[12..14], id.to_le_bytes());
        let mut reader = png::Decoder::new(std::io::Cursor::new(bytes))
            .read_info()
            .unwrap();
        let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut decoded).unwrap();
        assert_eq!((frame.width, frame.height), (size, size));
        // 浮点运算在不同的处理器上有细微差别，允许每个分量差一点。
        let expected = picture.render(size, size).rgba;
        let same = decoded.len() == expected.len()
            && decoded
                .iter()
                .zip(&expected)
                .all(|(a, b)| a.abs_diff(*b) <= 2);
        assert!(
            same,
            "重新生成 crates/winc-app/assets/winc.res（cargo xtask icons）：{size}"
        );
    }
}

#[test]
fn the_generated_icons_use_the_same_images() {
    let dir = std::env::temp_dir().join(format!("winc-icons-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    write(&dir).unwrap();
    let png = std::fs::read(dir.join("winc.png")).unwrap();
    let ico = std::fs::read(dir.join("winc.ico")).unwrap();
    let res = std::fs::read(dir.join("winc.res")).unwrap();
    for name in ["winc.png", "winc.ico", "winc.res"] {
        std::fs::remove_file(dir.join(name)).unwrap();
    }
    std::fs::remove_dir(dir).unwrap();

    let records = resource_records(&res);
    let count = usize::from(u16::from_le_bytes([ico[4], ico[5]]));
    assert_eq!(count, 4);
    assert_eq!(records.len(), count + 2);
    let group = records[count + 1].2;
    for index in 0..count {
        let entry = &ico[6 + 16 * index..6 + 16 * (index + 1)];
        let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        let bytes = &ico[offset..offset + size];
        let group_entry = &group[6 + 14 * index..6 + 14 * (index + 1)];
        assert_eq!(records[index + 1], (3, index as u16 + 1, bytes));
        assert_eq!(group_entry[..12], entry[..12]);
        assert_eq!(group_entry[12..14], (index as u16 + 1).to_le_bytes());
    }
    assert_eq!(records[count].2, png);
}

#[test]
fn a_file_that_is_not_an_icon_is_rejected() {
    assert_eq!(resource::icon_resources(b"not an icon", ICON_GROUP), None);
    // 目录项指向文件之外。
    let mut ico = vec![0, 0, 1, 0, 1, 0];
    ico.extend([0; 8]);
    ico.extend(100u32.to_le_bytes());
    ico.extend(22u32.to_le_bytes());
    assert_eq!(resource::icon_resources(&ico, ICON_GROUP), None);
}

/// macOS 图标的底板位于模板规定的位置：在 1024 × 1024 的画布上距各边约 100 像素；`ICON_TILE`
/// 与 SVG 中底板的宽度相同。
#[test]
fn the_macos_icon_follows_the_app_icon_template() {
    let text = std::fs::read_to_string(root().join("crates/winc-app/assets/icon.svg")).unwrap();
    assert!(text.contains(&format!("width=\"{}\"", crate::dmg::ICON_TILE)));
    let picture = winc_preview::svg::load(&text, None, None).unwrap();
    let pixels = crate::dmg::macos_icon(&picture, 1024);
    assert_eq!((pixels.width, pixels.height), (1024, 1024));
    let opaque = |x: usize, y: usize| pixels.rgba[(y * 1024 + x) * 4 + 3] > 128;
    let left = (0..1024).find(|&x| opaque(x, 512)).unwrap();
    let right = (0..1024).rev().find(|&x| opaque(x, 512)).unwrap();
    let top = (0..1024).find(|&y| opaque(512, y)).unwrap();
    for edge in [left, 1023 - right, top] {
        assert!((98..=102).contains(&edge), "{left} {right} {top}");
    }
}
