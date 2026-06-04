use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, ClickEvent, Entity, EventEmitter, FocusHandle, Focusable, Hsla, SharedString, Task,
    Window, actions, hsla, px,
};
use strum::IntoEnumIterator;
use theme::{ActiveTheme, ColorCategory, GlobalTheme, Theme, ThemeColorField};
use ui::prelude::*;
use ui::{Label, LabelCommon, LabelSize};

use crate::{Item, Workspace};

actions!(
    theme_customizer,
    [
        /// Opens the theme color customizer.
        OpenThemeCustomizer
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &OpenThemeCustomizer, window, cx| {
            let customizer = cx.new(|cx| ThemeCustomizer::new(window, cx));
            workspace.add_item_to_active_pane(Box::new(customizer), None, true, window, cx)
        });
    })
    .detach();
}

struct ThemeCustomizer {
    focus_handle: FocusHandle,
    base_theme: Arc<Theme>,
    color_overrides: HashMap<ThemeColorField, Hsla>,
    selected_field: Option<ThemeColorField>,
    active_category: Option<ColorCategory>,
    show_lathe_only: bool,
}

impl ThemeCustomizer {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let base_theme = cx.theme().clone();
        Self {
            focus_handle: cx.focus_handle(),
            base_theme,
            color_overrides: HashMap::new(),
            selected_field: None,
            active_category: None,
            show_lathe_only: false,
        }
    }

    fn current_color(&self, field: ThemeColorField) -> Hsla {
        self.color_overrides
            .get(&field)
            .copied()
            .unwrap_or_else(|| self.base_theme.styles.colors.color(field))
    }

    fn set_color(&mut self, field: ThemeColorField, color: Hsla, cx: &mut Context<Self>) {
        self.color_overrides.insert(field, color);
        self.apply_overrides(cx);
    }

    fn reset_color(&mut self, field: ThemeColorField, cx: &mut Context<Self>) {
        self.color_overrides.remove(&field);
        self.apply_overrides(cx);
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        self.color_overrides.clear();
        self.apply_overrides(cx);
    }

    fn apply_overrides(&self, cx: &mut Context<Self>) {
        let mut theme = (*self.base_theme).clone();
        for (&field, &color) in &self.color_overrides {
            theme.styles.colors.set_color(field, color);
        }
        GlobalTheme::update_theme(cx, Arc::new(theme));
        cx.refresh_windows();
        cx.notify();
    }

    fn filtered_fields(&self) -> Vec<ThemeColorField> {
        ThemeColorField::iter()
            .filter(|field| {
                if self.show_lathe_only {
                    return field.is_lathe_custom();
                }
                if let Some(category) = self.active_category {
                    if field.category() != category {
                        return false;
                    }
                }
                true
            })
            .collect()
    }

    fn render_category_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut row = h_flex()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .flex_wrap();

        // "All" tab
        let is_active = self.active_category.is_none() && !self.show_lathe_only;
        row = row.child(Self::category_pill("cat-all", "All", is_active, false, cx, {
            move |this: &mut Self, _: &ClickEvent, _window, cx: &mut Context<Self>| {
                this.active_category = None;
                this.show_lathe_only = false;
                cx.notify();
            }
        }));

        // "Lathe" tab
        row = row.child(Self::category_pill(
            "cat-lathe",
            "Lathe",
            self.show_lathe_only,
            true,
            cx,
            {
                move |this: &mut Self, _: &ClickEvent, _window, cx: &mut Context<Self>| {
                    this.show_lathe_only = !this.show_lathe_only;
                    if this.show_lathe_only {
                        this.active_category = None;
                    }
                    cx.notify();
                }
            },
        ));

        for category in ColorCategory::iter() {
            if category == ColorCategory::Other {
                continue;
            }
            let is_active = self.active_category == Some(category) && !self.show_lathe_only;
            let label = category.label();
            row = row.child(Self::category_pill(
                SharedString::from(format!("cat-{label}")),
                label,
                is_active,
                false,
                cx,
                move |this: &mut Self, _: &ClickEvent, _window, cx: &mut Context<Self>| {
                    if this.active_category == Some(category) {
                        this.active_category = None;
                    } else {
                        this.active_category = Some(category);
                        this.show_lathe_only = false;
                    }
                    cx.notify();
                },
            ));
        }

        row
    }

    fn category_pill(
        id: impl Into<ElementId>,
        label: &str,
        is_active: bool,
        is_accent: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .id(id.into())
            .px_2()
            .py_0p5()
            .rounded_md()
            .cursor_pointer()
            .when(is_active, |this| {
                this.bg(cx.theme().colors().element_selected)
            })
            .when(!is_active, |this| {
                this.hover(|style| style.bg(cx.theme().colors().element_hover))
            })
            .on_click(cx.listener(on_click))
            .child(
                Label::new(label.to_string())
                    .size(LabelSize::Small)
                    .color(if is_active {
                        Color::Default
                    } else if is_accent {
                        Color::Accent
                    } else {
                        Color::Muted
                    }),
            )
    }

    fn render_color_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let fields = self.filtered_fields();

        let mut list = v_flex()
            .id("color-list")
            .flex_grow(1.0)
            .overflow_y_scroll()
            .px_1()
            .py_1();

        for field in fields {
            let color = self.current_color(field);
            let is_selected = self.selected_field == Some(field);
            let is_overridden = self.color_overrides.contains_key(&field);
            let is_lathe = field.is_lathe_custom();

            list = list.child(
                div()
                    .id(SharedString::from(format!("color-{}", field.as_ref())))
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .when(is_selected, |this| {
                        this.bg(cx.theme().colors().element_selected)
                    })
                    .when(!is_selected, |this| {
                        this.hover(|style| style.bg(cx.theme().colors().element_hover))
                    })
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.selected_field = Some(field);
                        cx.notify();
                    }))
                    .child(color_swatch(color, px(20.0), cx))
                    .child(
                        v_flex()
                            .flex_grow(1.0)
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Label::new(field.display_name())
                                            .size(LabelSize::Small)
                                            .color(if is_selected {
                                                Color::Default
                                            } else {
                                                Color::Muted
                                            }),
                                    )
                                    .when(is_lathe, |this| {
                                        this.child(
                                            Label::new("lathe")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Accent),
                                        )
                                    })
                                    .when(is_overridden, |this| {
                                        this.child(
                                            Label::new("modified")
                                                .size(LabelSize::XSmall)
                                                .color(Color::Warning),
                                        )
                                    }),
                            )
                            .child(
                                Label::new(format_hsla_short(color))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Disabled),
                            ),
                    ),
            );
        }

        list
    }

    fn render_color_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(field) = self.selected_field else {
            return v_flex()
                .flex_grow(1.0)
                .items_center()
                .justify_center()
                .child(
                    Label::new("Select a color to edit")
                        .size(LabelSize::Default)
                        .color(Color::Muted),
                )
                .into_any_element();
        };

        let color = self.current_color(field);
        let is_overridden = self.color_overrides.contains_key(&field);
        let original = self.base_theme.styles.colors.color(field);

        v_flex()
            .id("color-editor")
            .flex_grow(1.0)
            .p_3()
            .gap_3()
            .overflow_y_scroll()
            // Header
            .child(
                v_flex()
                    .gap_1()
                    .child(Label::new(field.display_name()).size(LabelSize::Large))
                    .child(
                        Label::new(SharedString::from(field.as_ref().to_string()))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            // Preview swatches
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("Current")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(color_swatch(color, px(48.0), cx)),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                Label::new("Original")
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(color_swatch(original, px(48.0), cx)),
                    ),
            )
            // HSLA display
            .child(
                Label::new(format_hsla(color))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            // Hue slider
            .child(self.render_slider("Hue", format!("{:.0}", color.h * 360.0), color.h, field, SliderChannel::Hue, cx))
            // Saturation slider
            .child(self.render_slider("Saturation", format!("{:.0}%", color.s * 100.0), color.s, field, SliderChannel::Saturation, cx))
            // Lightness slider
            .child(self.render_slider("Lightness", format!("{:.0}%", color.l * 100.0), color.l, field, SliderChannel::Lightness, cx))
            // Alpha slider
            .child(self.render_slider("Alpha", format!("{:.0}%", color.a * 100.0), color.a, field, SliderChannel::Alpha, cx))
            // Actions
            .child(
                h_flex()
                    .gap_2()
                    .pt_2()
                    .when(is_overridden, |this| {
                        this.child(
                            Button::new("reset-color", "Reset to Original")
                                .style(ButtonStyle::Filled)
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.reset_color(field, cx);
                                })),
                        )
                    })
                    .when(!self.color_overrides.is_empty(), |this| {
                        this.child(
                            Button::new("reset-all", "Reset All")
                                .style(ButtonStyle::Subtle)
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.reset_all(cx);
                                })),
                        )
                    }),
            )
            // Override count
            .when(!self.color_overrides.is_empty(), |this| {
                this.child(
                    Label::new(format!(
                        "{} color{} modified",
                        self.color_overrides.len(),
                        if self.color_overrides.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ))
                    .size(LabelSize::XSmall)
                    .color(Color::Disabled),
                )
            })
            .into_any_element()
    }

    fn render_slider(
        &self,
        label: &str,
        value_text: String,
        value: f32,
        field: ThemeColorField,
        channel: SliderChannel,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let color = self.current_color(field);
        let gradient_stops = channel.gradient_stops(color);

        let step_small = match channel {
            SliderChannel::Hue => 1.0 / 360.0,
            _ => 0.01,
        };

        // Number of clickable segments in the gradient bar
        let segment_count = 50;

        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        Label::new(label.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(value_text)
                            .size(LabelSize::XSmall)
                            .color(Color::Disabled),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    // Decrement button
                    .child(
                        IconButton::new(
                            SharedString::from(format!("dec-{label}")),
                            IconName::ChevronLeft,
                        )
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            let mut current = this.current_color(field);
                            let val = channel.get_value(current);
                            channel.set_value(&mut current, (val - step_small).clamp(0.0, 1.0));
                            this.set_color(field, current, cx);
                        })),
                    )
                    // Clickable gradient segments
                    .child(
                        div()
                            .relative()
                            .flex_grow(1.0)
                            .h(px(28.0))
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().colors().border)
                            .overflow_hidden()
                            .child(
                                h_flex()
                                    .size_full()
                                    .children((0..segment_count).map(|i| {
                                        let t = (i as f32 + 0.5) / segment_count as f32;
                                        // Interpolate color for this segment from gradient stops
                                        let stop_index_f = i as f32
                                            / segment_count as f32
                                            * (gradient_stops.len() - 1) as f32;
                                        let stop_low = stop_index_f.floor() as usize;
                                        let stop_high = (stop_low + 1)
                                            .min(gradient_stops.len() - 1);
                                        let frac = stop_index_f - stop_low as f32;
                                        let bg_color = lerp_hsla(
                                            gradient_stops[stop_low],
                                            gradient_stops[stop_high],
                                            frac,
                                        );

                                        let is_thumb_here = (value - t).abs()
                                            < (0.5 / segment_count as f32);

                                        div()
                                            .id(SharedString::from(format!(
                                                "seg-{label}-{i}"
                                            )))
                                            .h_full()
                                            .flex_grow(1.0)
                                            .cursor_pointer()
                                            .bg(bg_color)
                                            .when(is_thumb_here, |this| {
                                                this.child(
                                                    div()
                                                        .size_full()
                                                        .border_l_2()
                                                        .border_r_2()
                                                        .border_color(
                                                            gpui::rgb(0xffffff),
                                                        ),
                                                )
                                            })
                                            .on_click(cx.listener(
                                                move |this, _, _window, cx| {
                                                    let mut current =
                                                        this.current_color(field);
                                                    channel
                                                        .set_value(&mut current, t);
                                                    this.set_color(
                                                        field, current, cx,
                                                    );
                                                },
                                            ))
                                    })),
                            ),
                    )
                    // Increment button
                    .child(
                        IconButton::new(
                            SharedString::from(format!("inc-{label}")),
                            IconName::ChevronRight,
                        )
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            let mut current = this.current_color(field);
                            let val = channel.get_value(current);
                            channel.set_value(&mut current, (val + step_small).clamp(0.0, 1.0));
                            this.set_color(field, current, cx);
                        })),
                    ),
            )
    }
}

#[derive(Clone, Copy)]
enum SliderChannel {
    Hue,
    Saturation,
    Lightness,
    Alpha,
}

impl SliderChannel {
    fn get_value(self, color: Hsla) -> f32 {
        match self {
            Self::Hue => color.h,
            Self::Saturation => color.s,
            Self::Lightness => color.l,
            Self::Alpha => color.a,
        }
    }

    fn set_value(self, color: &mut Hsla, value: f32) {
        match self {
            Self::Hue => color.h = value,
            Self::Saturation => color.s = value,
            Self::Lightness => color.l = value,
            Self::Alpha => color.a = value,
        }
    }

    fn gradient_stops(self, color: Hsla) -> Vec<Hsla> {
        let steps = 12;
        (0..=steps)
            .map(|i| {
                let t = i as f32 / steps as f32;
                match self {
                    Self::Hue => hsla(t, color.s.max(0.5), color.l.clamp(0.3, 0.7), 1.0),
                    Self::Saturation => hsla(color.h, t, color.l, 1.0),
                    Self::Lightness => hsla(color.h, color.s, t, 1.0),
                    Self::Alpha => hsla(color.h, color.s, color.l, t),
                }
            })
            .collect()
    }
}

fn lerp_hsla(a: Hsla, b: Hsla, t: f32) -> Hsla {
    hsla(
        a.h + (b.h - a.h) * t,
        a.s + (b.s - a.s) * t,
        a.l + (b.l - a.l) * t,
        a.a + (b.a - a.a) * t,
    )
}

fn color_swatch(color: Hsla, size: gpui::Pixels, cx: &Context<ThemeCustomizer>) -> impl IntoElement {
    div()
        .size(size)
        .rounded_sm()
        .border_1()
        .border_color(cx.theme().colors().border)
        .bg(gpui::rgb(0xcccccc))
        .child(div().size_full().rounded_sm().bg(color))
}

fn format_hsla(color: Hsla) -> String {
    format!(
        "H:{:.0} S:{:.0}% L:{:.0}% A:{:.0}%",
        color.h * 360.0,
        color.s * 100.0,
        color.l * 100.0,
        color.a * 100.0
    )
}

fn format_hsla_short(color: Hsla) -> String {
    format!(
        "{:.0} {:.0}% {:.0}% {:.0}%",
        color.h * 360.0,
        color.s * 100.0,
        color.l * 100.0,
        color.a * 100.0
    )
}

impl EventEmitter<()> for ThemeCustomizer {}

impl Focusable for ThemeCustomizer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ThemeCustomizer {
    type Event = ();

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(crate::item::ItemEvent)) {}

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Theme Customizer".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Sliders))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<crate::WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| Self::new(window, cx))))
    }
}

impl Render for ThemeCustomizer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .bg(cx.theme().colors().editor_background)
            .text_color(cx.theme().colors().text)
            .child(
                // Left panel: color list
                v_flex()
                    .w(px(340.0))
                    .h_full()
                    .border_r_1()
                    .border_color(cx.theme().colors().border)
                    .flex_shrink_0()
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(Label::new("Theme Colors").size(LabelSize::Default)),
                    )
                    .child(self.render_category_tabs(cx))
                    .child(self.render_color_list(cx)),
            )
            .child(
                // Right panel: color editor
                v_flex()
                    .flex_grow(1.0)
                    .h_full()
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .border_b_1()
                            .border_color(cx.theme().colors().border)
                            .child(
                                Label::new(
                                    self.selected_field
                                        .map(|f| f.display_name())
                                        .unwrap_or_else(|| "No selection".to_string()),
                                )
                                .size(LabelSize::Default)
                                .color(if self.selected_field.is_some() {
                                    Color::Default
                                } else {
                                    Color::Muted
                                }),
                            ),
                    )
                    .child(self.render_color_editor(cx)),
            )
    }
}
