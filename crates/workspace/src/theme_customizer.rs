use std::collections::HashMap;
use std::mem;
use std::sync::Arc;
use std::time::Duration;

use fs::Fs;
use gpui::{
    App, ClickEvent, Entity, EventEmitter, FocusHandle, Focusable, Hsla, SharedString,
    Subscription, Task, Window, actions, hsla, px,
};
use settings::Settings as _;
use strum::IntoEnumIterator;
use theme::{
    ActiveTheme, ColorCategory, CustomizableColor, GlobalTheme, Theme, ThemeRegistry, ThemeStyles,
};
use theme_settings::{ThemeSettings, ThemeStyleContent};
use ui::prelude::*;
use ui::{Label, LabelCommon, LabelSize};
use ui_input::{ErasedEditorEvent, InputField};
use util::ResultExt as _;
use zed_actions::search::FocusSearch;

use crate::{Item, Workspace};

/// Slider drags produce a stream of edits, so saving waits for a pause rather
/// than rewriting settings.json on every step.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

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
            let fs = workspace.app_state().fs.clone();
            let customizer = cx.new(|cx| ThemeCustomizer::new(fs, window, cx));
            workspace.add_item_to_active_pane(Box::new(customizer), None, true, window, cx)
        });
    })
    .detach();
}

struct ThemeCustomizer {
    focus_handle: FocusHandle,
    fs: Arc<dyn Fs>,
    /// The active theme as registered, before `theme_overrides` are applied.
    base_theme: Arc<Theme>,
    /// Colors saved in `theme_overrides`, plus edits that aren't saved yet.
    color_overrides: HashMap<CustomizableColor, Hsla>,
    /// Edits waiting to be saved; `None` removes the saved override.
    unsaved_changes: HashMap<CustomizableColor, Option<Hsla>>,
    save_task: Option<Task<()>>,
    selected_field: Option<CustomizableColor>,
    active_category: Option<ColorCategory>,
    show_lathe_only: bool,
    search_input: Entity<InputField>,
    search_query: String,
    _theme_subscription: Subscription,
    _search_subscription: Subscription,
}

impl ThemeCustomizer {
    fn new(fs: Arc<dyn Fs>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.on_release(|this, cx| this.save(cx)).detach();
        let search_input = cx.new(|cx| {
            InputField::new(window, cx, "Search colors by name or key")
                .start_icon(IconName::MagnifyingGlass)
        });
        let search_subscription = {
            let this = cx.weak_entity();
            let editor = search_input.read(cx).editor().clone();
            editor.subscribe(
                Box::new(move |event, _window, cx| {
                    if matches!(event, ErasedEditorEvent::BufferEdited) {
                        this.update(cx, |this, cx| this.search_query_changed(cx))
                            .log_err();
                    }
                }),
                window,
                cx,
            )
        };
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            fs,
            base_theme: registered_theme(cx),
            color_overrides: HashMap::new(),
            unsaved_changes: HashMap::new(),
            save_task: None,
            selected_field: None,
            active_category: None,
            show_lathe_only: false,
            search_input,
            search_query: String::new(),
            _theme_subscription: cx.observe_global::<GlobalTheme>(Self::sync_with_theme),
            _search_subscription: search_subscription,
        };
        this.load_saved_overrides(cx);
        this
    }

    /// Runs whenever the global theme changes, which covers switching themes
    /// and reloading after settings.json changes.
    fn sync_with_theme(&mut self, cx: &mut Context<Self>) {
        if cx.theme().name != self.base_theme.name {
            self.save(cx);
            self.base_theme = registered_theme(cx);
            self.selected_field = None;
        }
        self.load_saved_overrides(cx);
    }

    fn load_saved_overrides(&mut self, cx: &mut Context<Self>) {
        let mut color_overrides: HashMap<_, _> = ThemeSettings::get_global(cx)
            .theme_overrides
            .get(self.base_theme.name.as_ref())
            .map(|overrides| {
                theme_settings::customizable_color_overrides(overrides, &self.base_theme.styles)
            })
            .unwrap_or_default()
            .into_iter()
            .collect();
        for (&field, &color) in &self.unsaved_changes {
            match color {
                Some(color) => color_overrides.insert(field, color),
                None => color_overrides.remove(&field),
            };
        }
        self.color_overrides = color_overrides;
        cx.notify();
    }

    fn current_color(&self, field: CustomizableColor) -> Hsla {
        self.color_overrides
            .get(&field)
            .copied()
            .unwrap_or_else(|| self.base_theme.styles.customizable_color(field))
    }

    fn set_color(&mut self, field: CustomizableColor, color: Hsla, cx: &mut Context<Self>) {
        self.color_overrides.insert(field, color);
        self.unsaved_changes.insert(field, Some(color));
        self.apply_unsaved_changes(cx);
        self.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            this.update(cx, |this, cx| this.save(cx)).log_err();
        }));
    }

    fn reset_color(&mut self, field: CustomizableColor, cx: &mut Context<Self>) {
        self.color_overrides.remove(&field);
        self.unsaved_changes.insert(field, None);
        self.apply_unsaved_changes(cx);
        self.save(cx);
    }

    fn reset_all(&mut self, cx: &mut Context<Self>) {
        self.unsaved_changes
            .extend(self.color_overrides.drain().map(|(field, _)| (field, None)));
        self.apply_unsaved_changes(cx);
        self.save(cx);
    }

    /// Previews edits right away. Saving them triggers a theme reload from
    /// settings, which ends up with the same colors.
    fn apply_unsaved_changes(&self, cx: &mut Context<Self>) {
        let mut theme = (**cx.theme()).clone();
        for (&field, &color) in &self.unsaved_changes {
            let color = color.unwrap_or_else(|| self.base_theme.styles.customizable_color(field));
            theme.styles.set_customizable_color(field, color);
        }
        GlobalTheme::update_theme(cx, Arc::new(theme));
        cx.refresh_windows();
        cx.notify();
    }

    fn save(&mut self, cx: &App) {
        self.save_task = None;
        if self.unsaved_changes.is_empty() {
            return;
        }
        let changes = mem::take(&mut self.unsaved_changes);
        let theme = self.base_theme.clone();
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let theme_name = theme.name.to_string();
            let theme_overrides = &mut settings.theme.theme_overrides;
            let overrides = theme_overrides.entry(theme_name.clone()).or_default();
            for (field, color) in changes {
                theme_settings::set_customizable_color_override(
                    overrides,
                    field,
                    color,
                    &theme.styles,
                )
                .log_err();
            }
            if *overrides == ThemeStyleContent::default() {
                theme_overrides.remove(&theme_name);
            }
        });
    }

    fn search_query_changed(&mut self, cx: &mut Context<Self>) {
        self.search_query = self.search_input.read(cx).text(cx);
        // Search spans every category, so a highlighted category pill would
        // misdescribe the list.
        if !self.search_query.trim().is_empty() {
            self.active_category = None;
        }
        cx.notify();
    }

    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_query.is_empty() {
            let editor = self.search_input.read(cx).editor().clone();
            editor.clear(window, cx);
        }
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.search_input.read(cx).editor().clone();
        editor.focus_handle(cx).focus(window, cx);
        editor.select_all(window, cx);
    }

    /// Escape clears the query first, then leaves the search field.
    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_query.is_empty() {
            self.clear_search(window, cx);
        } else if self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
        {
            self.focus_handle.focus(window, cx);
        } else {
            cx.propagate();
        }
    }

    fn filtered_fields(&self) -> Vec<CustomizableColor> {
        let filter = ColorFilter {
            lathe_only: self.show_lathe_only,
            category: self.active_category,
            query: &self.search_query,
        };
        let styles = &self.base_theme.styles;
        styles
            .all_customizable_colors()
            .into_iter()
            .filter(|field| filter.matches(*field, styles))
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
        row = row.child(Self::category_pill(
            "cat-all",
            "All",
            is_active,
            false,
            cx,
            {
                move |this: &mut Self, _: &ClickEvent, _window, cx: &mut Context<Self>| {
                    this.active_category = None;
                    this.show_lathe_only = false;
                    cx.notify();
                }
            },
        ));

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
                move |this: &mut Self, _: &ClickEvent, window, cx: &mut Context<Self>| {
                    if this.active_category == Some(category) {
                        this.active_category = None;
                    } else {
                        // A category only describes the list without a
                        // query, so picking one ends the search.
                        this.clear_search(window, cx);
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

    fn render_search(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(div().flex_1().child(self.search_input.clone()))
    }

    fn render_color_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let fields = self.filtered_fields();
        if fields.is_empty() {
            return v_flex()
                .flex_grow(1.0)
                .p_3()
                .child(
                    Label::new("No colors match")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element();
        }

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
                    .id(SharedString::from(format!(
                        "color-{}",
                        field.key(&self.base_theme.styles)
                    )))
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
                                        Label::new(field.display_name(&self.base_theme.styles))
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

        list.into_any_element()
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
        let original = self.base_theme.styles.customizable_color(field);

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
                    .child(
                        Label::new(field.display_name(&self.base_theme.styles))
                            .size(LabelSize::Large),
                    )
                    .child(
                        Label::new(SharedString::from(field.key(&self.base_theme.styles)))
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
            .child(self.render_slider(
                "Hue",
                format!("{:.0}", color.h * 360.0),
                color.h,
                field,
                SliderChannel::Hue,
                cx,
            ))
            // Saturation slider
            .child(self.render_slider(
                "Saturation",
                format!("{:.0}%", color.s * 100.0),
                color.s,
                field,
                SliderChannel::Saturation,
                cx,
            ))
            // Lightness slider
            .child(self.render_slider(
                "Lightness",
                format!("{:.0}%", color.l * 100.0),
                color.l,
                field,
                SliderChannel::Lightness,
                cx,
            ))
            // Alpha slider
            .child(self.render_slider(
                "Alpha",
                format!("{:.0}%", color.a * 100.0),
                color.a,
                field,
                SliderChannel::Alpha,
                cx,
            ))
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
        field: CustomizableColor,
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
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                let mut current = this.current_color(field);
                                let val = channel.get_value(current);
                                channel.set_value(&mut current, (val - step_small).clamp(0.0, 1.0));
                                this.set_color(field, current, cx);
                            },
                        )),
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
                            .child(h_flex().size_full().children((0..segment_count).map(|i| {
                                let t = (i as f32 + 0.5) / segment_count as f32;
                                // Interpolate color for this segment from gradient stops
                                let stop_index_f = i as f32 / segment_count as f32
                                    * (gradient_stops.len() - 1) as f32;
                                let stop_low = stop_index_f.floor() as usize;
                                let stop_high = (stop_low + 1).min(gradient_stops.len() - 1);
                                let frac = stop_index_f - stop_low as f32;
                                let bg_color = lerp_hsla(
                                    gradient_stops[stop_low],
                                    gradient_stops[stop_high],
                                    frac,
                                );

                                let is_thumb_here =
                                    (value - t).abs() < (0.5 / segment_count as f32);

                                div()
                                    .id(SharedString::from(format!("seg-{label}-{i}")))
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
                                                .border_color(gpui::rgb(0xffffff)),
                                        )
                                    })
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        let mut current = this.current_color(field);
                                        channel.set_value(&mut current, t);
                                        this.set_color(field, current, cx);
                                    }))
                            }))),
                    )
                    // Increment button
                    .child(
                        IconButton::new(
                            SharedString::from(format!("inc-{label}")),
                            IconName::ChevronRight,
                        )
                        .size(ButtonSize::Compact)
                        .on_click(cx.listener(
                            move |this, _, _window, cx| {
                                let mut current = this.current_color(field);
                                let val = channel.get_value(current);
                                channel.set_value(&mut current, (val + step_small).clamp(0.0, 1.0));
                                this.set_color(field, current, cx);
                            },
                        )),
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

/// What the color list shows. Each part narrows it further: Lathe-only and the
/// query combine, and a category is only set when there is no query.
struct ColorFilter<'a> {
    lathe_only: bool,
    category: Option<ColorCategory>,
    query: &'a str,
}

impl ColorFilter<'_> {
    fn matches(&self, field: CustomizableColor, styles: &ThemeStyles) -> bool {
        if self.lathe_only && !field.is_lathe_custom() {
            return false;
        }
        if self
            .category
            .is_some_and(|category| field.category() != category)
        {
            return false;
        }
        let query = self.query.trim();
        if query.is_empty() {
            return true;
        }
        // Keys are matched too, so a key copied out of settings.json finds
        // its color.
        let query = query.to_lowercase();
        field.display_name(styles).to_lowercase().contains(&query)
            || field.key(styles).to_lowercase().contains(&query)
    }
}

fn registered_theme(cx: &App) -> Arc<Theme> {
    let active_theme = cx.theme().clone();
    ThemeRegistry::global(cx)
        .get(&active_theme.name)
        .log_err()
        .unwrap_or(active_theme)
}

fn lerp_hsla(a: Hsla, b: Hsla, t: f32) -> Hsla {
    hsla(
        a.h + (b.h - a.h) * t,
        a.s + (b.s - a.s) * t,
        a.l + (b.l - a.l) * t,
        a.a + (b.a - a.a) * t,
    )
}

fn color_swatch(
    color: Hsla,
    size: gpui::Pixels,
    cx: &Context<ThemeCustomizer>,
) -> impl IntoElement {
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
        let fs = self.fs.clone();
        Task::ready(Some(cx.new(|cx| Self::new(fs, window, cx))))
    }
}

impl Render for ThemeCustomizer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .key_context("ThemeCustomizer")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_search))
            .on_action(cx.listener(Self::cancel))
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
                    .child(self.render_search(cx))
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
                                        .map(|f| f.display_name(&self.base_theme.styles))
                                        .unwrap_or_else(|| "No selection".to_string()),
                                )
                                .size(LabelSize::Default)
                                .color(
                                    if self.selected_field.is_some() {
                                        Color::Default
                                    } else {
                                        Color::Muted
                                    },
                                ),
                            ),
                    )
                    .child(self.render_color_editor(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use theme::StatusColorField;

    fn default_styles() -> ThemeStyles {
        let registry = ThemeRegistry::new(Box::new(()));
        let name = registry
            .list_names()
            .into_iter()
            .next()
            .expect("the registry always holds the fallback theme");
        let theme = registry.get(&name).expect("listed theme exists");
        theme.styles.clone()
    }

    fn matching(styles: &ThemeStyles, filter: &ColorFilter) -> Vec<CustomizableColor> {
        styles
            .all_customizable_colors()
            .into_iter()
            .filter(|field| filter.matches(*field, styles))
            .collect()
    }

    #[test]
    fn query_matches_keys_and_names_without_case() {
        let styles = default_styles();
        let by_key = matching(
            &styles,
            &ColorFilter {
                lathe_only: false,
                category: None,
                query: "status.error",
            },
        );
        assert!(by_key.contains(&CustomizableColor::Status(StatusColorField::Error)));
        assert!(by_key.iter().all(|field| {
            field.key(&styles).contains("status.error")
                || field
                    .display_name(&styles)
                    .to_lowercase()
                    .contains("status.error")
        }));

        let shouted = matching(
            &styles,
            &ColorFilter {
                lathe_only: false,
                category: None,
                query: "  STATUS.ERROR ",
            },
        );
        assert_eq!(shouted, by_key);

        let error_field = CustomizableColor::Status(StatusColorField::Error);
        let name = error_field.display_name(&styles);
        let by_name = matching(
            &styles,
            &ColorFilter {
                lathe_only: false,
                category: None,
                query: &name.to_uppercase(),
            },
        );
        assert!(by_name.contains(&error_field));
    }

    #[test]
    fn empty_query_filters_nothing_and_unknown_query_matches_nothing() {
        let styles = default_styles();
        let everything = styles.all_customizable_colors();
        let blank = matching(
            &styles,
            &ColorFilter {
                lathe_only: false,
                category: None,
                query: "   ",
            },
        );
        assert_eq!(blank, everything);

        let nothing = matching(
            &styles,
            &ColorFilter {
                lathe_only: false,
                category: None,
                query: "no color is called this",
            },
        );
        assert!(nothing.is_empty());
    }

    #[test]
    fn lathe_only_composes_with_query() {
        let styles = default_styles();
        let all_tab = matching(
            &styles,
            &ColorFilter {
                lathe_only: false,
                category: None,
                query: "tab",
            },
        );
        let lathe_tab = matching(
            &styles,
            &ColorFilter {
                lathe_only: true,
                category: None,
                query: "tab",
            },
        );
        assert!(!lathe_tab.is_empty());
        assert!(lathe_tab.iter().all(CustomizableColor::is_lathe_custom));
        assert!(lathe_tab.iter().all(|field| all_tab.contains(field)));
        assert!(
            lathe_tab.len() < all_tab.len(),
            "non-Lathe tab colors are left out"
        );
    }

    #[test]
    fn category_narrows_without_query() {
        let styles = default_styles();
        let status = matching(
            &styles,
            &ColorFilter {
                lathe_only: false,
                category: Some(ColorCategory::Status),
                query: "",
            },
        );
        assert!(!status.is_empty());
        assert!(
            status
                .iter()
                .all(|field| field.category() == ColorCategory::Status)
        );
    }
}
