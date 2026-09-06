use std::{hint::black_box, time::Instant};

use touchbar_ui::{
    CrossAxisAlignment, Flex, FlexItem, Icon, Image, ImageFit, ImageTint, Node, Representation,
    ResponsiveVariant, RetainedUi, Theme, WidgetId,
};

const ITERATIONS: u32 = 5_000;

fn media_widget() -> Node {
    let artwork = Image::rgba8(100, 1, 32, 32, vec![0x80; 32 * 32 * 4])
        .expect("probe image dimensions are valid");
    Node::row_aligned(
        4.0,
        4.0,
        CrossAxisAlignment::Center,
        vec![
            FlexItem::new(
                Flex::fixed(32.0).priority(-1),
                Node::Image {
                    image: artwork,
                    opacity: 1.0,
                    fit: ImageFit::Cover,
                    tint: ImageTint::None,
                    label: "Artwork".into(),
                },
            ),
            FlexItem::new(
                Flex::flexible(24.0, 100.0, 300.0).grow(1.0).required(),
                Node::column_aligned(
                    2.0,
                    0.0,
                    CrossAxisAlignment::Start,
                    vec![
                        FlexItem::new(
                            Flex::content(10.0, 24.0),
                            Node::responsive(
                                WidgetId(1),
                                vec![
                                    ResponsiveVariant::new(
                                        Representation::Minimal,
                                        0.0,
                                        Node::label("PLAY", 10.0),
                                    ),
                                    ResponsiveVariant::new(
                                        Representation::Compact,
                                        64.0,
                                        Node::label("Touch Bar", 11.0),
                                    ),
                                    ResponsiveVariant::new(
                                        Representation::Full,
                                        150.0,
                                        Node::label("Touch Bar Layout Probe", 12.0),
                                    ),
                                ],
                            ),
                        ),
                        FlexItem::new(Flex::content(8.0, 20.0), Node::label("TouchBar", 9.0)),
                    ],
                ),
            ),
            FlexItem::new(
                Flex::fixed(40.0).required(),
                Node::button(WidgetId(2), "", Some(Icon::Play), false),
            ),
        ],
    )
}

fn main() {
    for width in [80.0, 160.0, 360.0] {
        let mut ui = RetainedUi::new(media_widget());
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(ui.resolve(
                touchbar_ui::Rect::new(0.0, 0.0, width, 60.0),
                Theme::default(),
            ));
        }
        let elapsed = started.elapsed();
        println!(
            "layout-probe width={width:.0} iterations={ITERATIONS} total_us={} ns_per_layout={}",
            elapsed.as_micros(),
            elapsed.as_nanos() / u128::from(ITERATIONS)
        );
    }
}
