//! Native geometry from the shared face domain. User outline, eyes and hue remain visible.
use gpui_kit::*;
use vibeface_client::{Avatar, Eyes, Outline};
const HUES: [u16; 12] = [20, 45, 100, 130, 155, 180, 205, 230, 255, 285, 315, 345];
pub fn face(id: &str, custom: Option<&Avatar>) -> Avatar {
    let mut hash = 2166136261u32;
    for unit in id.encode_utf16() {
        hash = (hash ^ u32::from(unit)).wrapping_mul(16777619);
    }
    hash ^= hash >> 16;
    let mut more = (hash ^ 7u32.wrapping_mul(0x9e3779b1)).wrapping_mul(0x85ebca6b);
    more ^= more >> 13;
    more = more.wrapping_mul(0xc2b2ae35);
    more ^= more >> 16;
    custom.cloned().unwrap_or_else(|| Avatar {
        hue: HUES[hash as usize % 12],
        outline: match (hash >> 3) % 5 {
            0 => Outline::Round,
            1 => Outline::Squircle,
            2 => Outline::Drop,
            3 => Outline::Bean,
            _ => Outline::Cloud,
        },
        eyes: match (more >> 4) % 4 {
            0 => Eyes::Dot,
            1 => Eyes::Pill,
            2 => Eyes::Oval,
            _ => Eyes::Wide,
        },
    })
}
pub fn render(id: &str, custom: Option<&Avatar>) -> AnyElement {
    let avatar = face(id, custom);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            let x = f32::from(bounds.origin.x) + width / 2.;
            let y = f32::from(bounds.origin.y) + height / 2.;
            let radius = width.min(height) * 0.4;
            let mut outline = PathBuilder::fill();
            for index in 0..48 {
                let angle = index as f32 * std::f32::consts::TAU / 48.;
                let rho = match &avatar.outline {
                    Outline::Round => 1.,
                    Outline::Squircle => {
                        (angle.cos().abs().powi(4) + angle.sin().abs().powi(4)).powf(-0.25) * 0.91
                    }
                    Outline::Drop => {
                        1. + 0.13 * (-angle.sin()).max(0.).powi(6) + 0.04 * angle.sin().max(0.)
                    }
                    Outline::Bean => 1. + 0.06 * (2. * angle).cos() - 0.05 * angle.sin(),
                    Outline::Cloud => 1. + 0.07 * ((3.5 * angle).cos().abs() - 0.6),
                };
                let point = point(
                    px(x + angle.cos() * radius * rho),
                    px(y + angle.sin() * radius * rho),
                );
                if index == 0 {
                    outline.move_to(point)
                } else {
                    outline.line_to(point)
                }
            }
            outline.close();
            if let Ok(path) = outline.build() {
                window.paint_path(path, hsla(avatar.hue as f32 / 360., 0.55, 0.72, 1.));
            }
            let (rx, ry) = match avatar.eyes {
                Eyes::Dot => (0.055, 0.055),
                Eyes::Pill => (0.055, 0.1),
                Eyes::Oval => (0.075, 0.1),
                Eyes::Wide => (0.095, 0.085),
            };
            for offset in [-0.14, 0.14] {
                let mut eye = PathBuilder::fill();
                for index in 0..24 {
                    let angle = index as f32 * std::f32::consts::TAU / 24.;
                    let p = point(
                        px(x + width * (offset + rx * angle.cos())),
                        px(y + height * ry * angle.sin()),
                    );
                    if index == 0 {
                        eye.move_to(p)
                    } else {
                        eye.line_to(p)
                    }
                }
                eye.close();
                if let Ok(path) = eye.build() {
                    window.paint_path(path, rgb(0x302635));
                }
            }
        },
    )
    .size(px(44.))
    .into_any_element()
}
