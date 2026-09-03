use image::{imageops::FilterType, GenericImageView, Rgba, RgbaImage};
use std::collections::VecDeque;

// Key out pale, low-saturation pixels reachable from the border (white bg + motion-blur arcs).
fn is_bg(p: &Rgba<u8>) -> bool {
    let (mx, mn) = (p.0[0].max(p.0[1]).max(p.0[2]), p.0[0].min(p.0[1]).min(p.0[2]));
    mn > 175 && mx - mn < 70
}
fn key_background(img: &mut RgbaImage) {
    let (w, h) = img.dimensions();
    let mut q = VecDeque::new();
    let mut seen = vec![false; (w * h) as usize];
    for x in 0..w { q.push_back((x, 0)); q.push_back((x, h - 1)); }
    for y in 0..h { q.push_back((0, y)); q.push_back((w - 1, y)); }
    while let Some((x, y)) = q.pop_front() {
        let i = (y * w + x) as usize;
        if seen[i] || !is_bg(img.get_pixel(x, y)) { continue; }
        seen[i] = true;
        img.put_pixel(x, y, Rgba([0, 0, 0, 0]));
        if x > 0 { q.push_back((x - 1, y)); } if x + 1 < w { q.push_back((x + 1, y)); }
        if y > 0 { q.push_back((x, y - 1)); } if y + 1 < h { q.push_back((x, y + 1)); }
    }
}

// Erode then dilate alpha by r px: removes strokes thinner than 2r (propeller blur arcs).
fn open_alpha(img: &mut RgbaImage, r: i32) {
    let (w, h) = img.dimensions();
    let a: Vec<bool> = img.pixels().map(|p| p.0[3] > 16).collect();
    let pass = |src: &Vec<bool>, keep_if_all: bool| -> Vec<bool> {
        let mut out = vec![false; src.len()];
        for y in 0..h as i32 { for x in 0..w as i32 {
            let mut all = true; let mut any = false;
            for dy in -r..=r { for dx in -r..=r {
                let (nx, ny) = (x+dx, y+dy);
                let v = nx>=0 && ny>=0 && nx<w as i32 && ny<h as i32 && src[(ny*w as i32+nx) as usize];
                all &= v; any |= v;
            }}
            out[(y*w as i32+x) as usize] = if keep_if_all { all } else { any };
        }}
        out
    };
    let eroded = pass(&a, true);
    let opened = pass(&eroded, false);
    for (i, p) in img.pixels_mut().enumerate() { if !opened[i] { *p = Rgba([0,0,0,0]); } }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let cols: u32 = a[2].parse().unwrap();
    let lum_weight: f32 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let mut img = image::open(&a[1]).unwrap().to_rgba8();
    key_background(&mut img);
    let open_r: i32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    if open_r > 0 { open_alpha(&mut img, open_r); }
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (x, y, p) in img.enumerate_pixels() { if p.0[3] > 16 { x0=x0.min(x); y0=y0.min(y); x1=x1.max(x); y1=y1.max(y); } }
    let img = image::DynamicImage::ImageRgba8(img).crop_imm(x0, y0, x1-x0+1, y1-y0+1);
    let (w, h) = img.dimensions();
    let rows = ((h as f32 / w as f32) * cols as f32 / 2.0).round() as u32;
    // Triangle filter averages alpha across the cell -> fractional coverage at edges.
    let mut pre = img.to_rgba8();
    for p in pre.pixels_mut() { let a = p.0[3] as u32; for c in 0..3 { p.0[c] = (p.0[c] as u32 * a / 255) as u8; } }
    let mut img = image::DynamicImage::ImageRgba8(pre).resize_exact(cols, rows, FilterType::Triangle).to_rgba8();
    for p in img.pixels_mut() { let a = p.0[3] as u32; if a > 0 { for c in 0..3 { p.0[c] = (p.0[c] as u32 * 255 / a).min(255) as u8; } } }
    let ramp: Vec<char> = std::env::var("RAMP").ok().map(|r| r.chars().collect()).unwrap_or(vec![' ', '░', '▒', '▓', '█']);
    let n = (ramp.len() - 1) as f32;
    // Propeller: drop source rows that are yellow-dominant, then draw a symmetric blade over the hub.
    let is_yellow = |p: &Rgba<u8>| p.0[3] > 40 && p.0[0] > p.0[1] && p.0[1] > p.0[2] && p.0[0] as f32 > 1.5 * p.0[2] as f32;
    let mut blade_rows = 0u32;
    for y in 0..rows { let yc = (0..cols).filter(|&x| is_yellow(&img.get_pixel(x, y))).count(); let tot = (0..cols).filter(|&x| img.get_pixel(x, y).0[3] > 40).count(); if tot > 0 && yc * 2 >= tot { blade_rows = y + 1; } else { break; } }
    let hub_row = blade_rows;
    let hub: Vec<u32> = (0..cols).filter(|&x| img.get_pixel(x, hub_row).0[3] > 100).collect();
    let hub_c = if hub.is_empty() { cols / 2 } else { (hub[0] + hub[hub.len()-1]) / 2 };
    let half: i32 = std::env::var("BLADE").ok().and_then(|v| v.parse().ok()).unwrap_or(9);
    let (br, bg, bb) = (243, 182, 64);
    let mut out = String::new();
    {
        let mut line = String::new();
        for x in 0..cols as i32 {
            let d = (x - hub_c as i32).abs();
            let ch = if d > half { ' ' } else if d > half - 2 { ramp[(n * 0.5).round() as usize] } else { ramp[ramp.len()-1] };
            if ch == ' ' { line.push(' '); } else { line.push_str(&format!("\x1b[38;2;{br};{bg};{bb}m{ch}")); }
        }
        out.push_str(line.trim_end()); out.push_str("\x1b[0m\n");
    }
    for y in blade_rows..rows {
        for x in 0..cols {
            let p = img.get_pixel(x, y);
            let cov = p.0[3] as f32 / 255.0;
            let lum = (0.2126*p.0[0] as f32 + 0.7152*p.0[1] as f32 + 0.0722*p.0[2] as f32) / 255.0;
            let d = cov * (1.0 - lum_weight * lum);
            let idx = ((d * n).round() as usize).min(ramp.len()-1);
            if idx == 0 || cov < 0.15 { out.push(' '); continue; }
            out.push_str(&format!("\x1b[38;2;{};{};{}m{}", p.0[0], p.0[1], p.0[2], ramp[idx]));
        }
        out.push_str("\x1b[0m\n");
    }
    print!("{out}");
}
