use {
    super::*,
    crate::{
        app::*,
        browser::BrowserState,
        command::{Command, ScrollCommand, TriggerType},
        display::*,
        errors::ProgramError,
        pattern::*,
        skin::PanelSkin,
        task_sync::Dam,
        tree::TreeOptions,
        verb::*,
    },
    crossterm::{
        cursor,
        style::Color,
        QueueableCommand,
    },
    lfs_core::Mount,
    minimad::Alignment,
    std::{
        convert::TryInto,
        fs,
        os::unix::fs::MetadataExt,
        path::Path,
    },
    strict::NonEmptyVec,
    termimad::{Area, ProgressBar},
};

struct FilteredContent {
    pattern: Pattern,
    mounts: Vec<Mount>, // may be empty
    selection_idx: usize,
}

/// an application state showing the currently mounted filesystems
pub struct FilesystemState {
    mounts: NonEmptyVec<Mount>,
    selection_idx: usize,
    scroll: usize,
    page_height: usize,
    tree_options: TreeOptions,
    filtered: Option<FilteredContent>,
}

impl FilesystemState {
    /// create a state listing the filesystem, trying to select
    /// the one containing the path given in argument.
    /// Not finding any filesystem is considered an error and prevents
    /// the opening of this state.
    pub fn new(
        path: &Path,
        tree_options: TreeOptions,
        _con: &AppContext,
    ) -> Result<FilesystemState, ProgramError> {
        let mut mount_list = MOUNTS.lock().unwrap();
        let show_only_disks = false;
        let mounts = mount_list
            .load()?
            .iter()
            .filter(|mount|
                if show_only_disks {
                    mount.disk.is_some()
                } else {
                    mount.stats.is_some()
                }
            )
            .cloned()
            .collect::<Vec<Mount>>();
        let mounts: NonEmptyVec<Mount> = match mounts.try_into() {
            Ok(nev) => nev,
            _ => {
                return Err(ProgramError::Lfs{details: "no disk in lfs-core list".to_string()});
            }
        };
        let device_id = fs::metadata(path)?.dev().into();
        let selection_idx = mounts.iter()
            .position(|m| m.info.dev == device_id)
            .unwrap_or(0);
        Ok(FilesystemState {
            mounts,
            selection_idx,
            scroll: 0,
            page_height: 0,
            tree_options,
            filtered: None,
        })
    }
    pub fn count(&self) -> usize {
        self.filtered.as_ref()
            .map(|f| f.mounts.len())
            .unwrap_or_else(|| self.mounts.len().into())
    }
    pub fn try_scroll(
        &mut self,
        cmd: ScrollCommand,
    ) -> bool {
        let old_scroll = self.scroll;
        self.scroll = cmd.apply(self.scroll, self.count(), self.page_height);
        self.scroll != old_scroll
    }
}

impl AppState for FilesystemState {

    fn selected_path(&self) -> &Path {
        &self.mounts[self.selection_idx].info.mount_point
    }

    fn tree_options(&self) -> TreeOptions {
        self.tree_options.clone()
    }

    fn with_new_options(
        &mut self,
        _screen: Screen,
        change_options: &dyn Fn(&mut TreeOptions),
        _in_new_panel: bool, // TODO open tree if true
        _con: &AppContext,
    ) -> AppStateCmdResult {
        change_options(&mut self.tree_options);
        AppStateCmdResult::Keep
    }

    fn selection(&self) -> Selection<'_> {
        Selection {
            path: self.selected_path(),
            stype: SelectionType::Directory,
            is_exe: false,
            line: 0,
        }
    }

    fn refresh(&mut self, _screen: Screen, _con: &AppContext) -> Command {
        Command::empty()
    }

    fn on_pattern(
        &mut self,
        pattern: InputPattern,
        _con: &AppContext,
    ) -> Result<AppStateCmdResult, ProgramError> {
        if pattern.is_none() {
            self.filtered = None;
        } else {
            let mut selection_idx = 0;
            let mut mounts = Vec::new();
            let pattern = pattern.pattern;
            for (idx, mount) in self.mounts.iter().enumerate() {
                if pattern.score_of_string(&mount.info.fs).is_none()
                    && mount.disk.as_ref().and_then(|d| pattern.score_of_string(d.disk_type())).is_none()
                    && pattern.score_of_string(&mount.info.fs_type).is_none()
                    && pattern.score_of_string(&mount.info.mount_point.to_string_lossy()).is_none()
                { continue; }
                if idx <= self.selection_idx {
                    selection_idx = mounts.len();
                }
                mounts.push(mount.clone());
            }
            self.filtered = Some(FilteredContent {
                pattern,
                mounts,
                selection_idx,
            });
        }
        Ok(AppStateCmdResult::Keep)
    }

    fn display(
        &mut self,
        w: &mut W,
        _screen: Screen,
        area: Area,
        panel_skin: &PanelSkin,
        con: &AppContext,
    ) -> Result<(), ProgramError> {
        self.page_height = area.height as usize;
        let (mounts, selection_idx) = if let Some(filtered) = &self.filtered {
            (filtered.mounts.as_slice(), filtered.selection_idx)
        } else {
            (self.mounts.as_slice(), self.selection_idx)
        };
        let scrollbar = area.scrollbar(self.scroll as i32, mounts.len() as i32);
        //- style preparation
        let styles = &panel_skin.styles;
        let selection_bg = styles.selected_line.get_bg()
            .unwrap_or(Color::AnsiValue(240));
        let match_style = &styles.char_match;
        let mut selected_match_style = styles.char_match.clone();
        selected_match_style.set_bg(selection_bg);
        let border_style = &styles.help_table_border;
        let mut selected_border_style = styles.help_table_border.clone();
        selected_border_style.set_bg(selection_bg);
        //- width computations and selection of columns to display
        let width = area.width as usize;
        let w_fs = mounts.iter()
            .map(|m| m.info.fs.chars().count())
            .max().unwrap_or(0)
            .max("filesystem".len());
        let mut wc_fs = w_fs; // width of the column (may include selection mark)
        if con.show_selection_mark {
            wc_fs += 1;
        }
        let w_dsk = 3;
        let w_type = mounts.iter()
            .map(|m| m.info.fs_type.chars().count())
            .max().unwrap_or(0)
            .max("type".len());
        let w_size = 4;
        let w_use = 4;
        let mut w_use_bar = 1; // min size, may grow if space available
        let w_use_share = 4;
        let mut wc_use = w_use; // sum of all the parts of the usage column
        let w_free = 4;
        let w_mount_point = mounts.iter()
            .map(|m| m.info.mount_point.to_string_lossy().chars().count())
            .max().unwrap_or(0)
            .max("mount point".len());
        let w_mandatory = wc_fs + 1 + w_size + 1 + w_free + 1 + w_mount_point;
        let mut e_dsk = false;
        let mut e_type = false;
        let mut e_use_bar = false;
        let mut e_use_share = false;
        let mut e_use = false;
        if w_mandatory + 1 < width {
            let mut rem = width - w_mandatory - 1;
            if rem > w_use {
                rem -= w_use + 1;
                e_use = true;
            }
            if e_use && rem > w_use_share {
                rem -= w_use_share; // no separation with use
                e_use_share = true;
                wc_use += w_use_share;
            }
            if rem > w_dsk {
                rem -= w_dsk + 1;
                e_dsk = true;
            }
            if e_use && rem > w_use_bar {
                rem -= w_use_bar + 1;
                e_use_bar = true;
                wc_use += w_use_bar + 1;
            }
            if rem > w_type {
                rem -= w_type + 1;
                e_type = true;
            }
            if e_use_bar && rem > 0 {
                let incr = rem.min(9);
                w_use_bar += incr;
                wc_use += incr;
            }
        }
        //- titles
        w.queue(cursor::MoveTo(area.left, area.top))?;
        let mut cw = CropWriter::new(w, width);
        cw.queue_g_string(&styles.default, format!("{:width$}", "filesystem", width = wc_fs))?;
        cw.queue_char(border_style, '│')?;
        if e_dsk {
            cw.queue_g_string(&styles.default, "dsk".to_string())?;
            cw.queue_char(border_style, '│')?;
        }
        if e_type {
            cw.queue_g_string(&styles.default, format!("{:^width$}", "type", width = w_type))?;
            cw.queue_char(border_style, '│')?;
        }
        cw.queue_g_string(&styles.default, "size".to_string())?;
        cw.queue_char(border_style, '│')?;
        if e_use {
            cw.queue_g_string(&styles.default, format!(
                "{:^width$}", if wc_use > 4 { "usage" } else { "use" }, width = wc_use
            ))?;
            cw.queue_char(border_style, '│')?;
        }
        cw.queue_g_string(&styles.default, "free".to_string())?;
        cw.queue_char(border_style, '│')?;
        cw.queue_g_string(&styles.default, "mount point".to_string())?;
        cw.fill(border_style, &SPACE_FILLING)?;
        //- horizontal line
        w.queue(cursor::MoveTo(area.left, 1 + area.top))?;
        let mut cw = CropWriter::new(w, width);
        cw.queue_g_string(border_style, format!("{:─>width$}", '┼', width = wc_fs+1))?;
        if e_dsk {
            cw.queue_g_string(border_style, format!("{:─>width$}", '┼', width = w_dsk+1))?;
        }
        if e_type {
            cw.queue_g_string(border_style, format!("{:─>width$}", '┼', width = w_type+1))?;
        }
        cw.queue_g_string(border_style, format!("{:─>width$}", '┼', width = w_size+1))?;
        if e_use {
            cw.queue_g_string(border_style, format!("{:─>width$}", '┼', width = wc_use+1))?;
        }
        cw.queue_g_string(border_style, format!("{:─>width$}", '┼', width = w_free+1))?;
        cw.fill(border_style, &BRANCH_FILLING)?;
        //- content
        let mut idx = self.scroll as usize;
        for y in 2..area.height {
            w.queue(cursor::MoveTo(area.left, y + area.top))?;
            let selected = selection_idx == idx;
            let mut cw = CropWriter::new(w, width - 1); // -1 for scrollbar
            let txt_style = if selected { &styles.selected_line } else { &styles.default };
            if let Some(mount) = mounts.get(idx) {
                let match_style = if selected { &selected_match_style } else { &match_style };
                let border_style = if selected { &selected_border_style } else { &border_style };
                if con.show_selection_mark {
                    cw.queue_char(&txt_style, if selected { '▶' } else { ' ' })?;
                }
                // fs
                let s = &mount.info.fs;
                let mut matched_string = MatchedString::new(
                    self.filtered.as_ref().and_then(|f| f.pattern.search_string(s)),
                    s,
                    txt_style,
                    match_style,
                );
                matched_string.fill(w_fs, Alignment::Left);
                matched_string.queue_on(&mut cw)?;
                cw.queue_char(border_style, '│')?;
                // dsk
                if e_dsk {
                    if let Some(disk) = mount.disk.as_ref() {
                        let s = disk.disk_type();
                        let mut matched_string = MatchedString::new(
                            self.filtered.as_ref().and_then(|f| f.pattern.search_string(s)),
                            s,
                            txt_style,
                            match_style,
                        );
                        matched_string.fill(3, Alignment::Left);
                        matched_string.queue_on(&mut cw)?;
                    } else {
                        cw.queue_g_string(txt_style, "   ".to_string())?;
                    }
                    cw.queue_char(border_style, '│')?;
                }
                // type
                if e_type {
                    let s = &mount.info.fs_type;
                    let mut matched_string = MatchedString::new(
                        self.filtered.as_ref().and_then(|f| f.pattern.search_string(s)),
                        s,
                        txt_style,
                        match_style,
                    );
                    matched_string.fill(w_type, Alignment::Center);
                    matched_string.queue_on(&mut cw)?;
                    cw.queue_char(border_style, '│')?;
                }
                // size, used, free
                if let Some(stats) = mount.stats.as_ref().filter(|s|s.size()>0) {
                    // size
                    cw.queue_g_string(txt_style, format!("{:>4}", file_size::fit_4(mount.size())))?;
                    cw.queue_char(border_style, '│')?;
                    // used
                    if e_use {
                        cw.queue_g_string(txt_style, format!("{:>4}", file_size::fit_4(stats.used())))?;
                        let share_color = super::share_color(stats.use_share());
                        if e_use_bar {
                            cw.queue_char(txt_style, ' ')?;
                            let pb = ProgressBar::new(stats.use_share() as f32, w_use_bar);
                            let mut bar_style = styles.default.clone();
                            bar_style.set_bg(share_color);
                            cw.queue_g_string(&bar_style, format!("{:<width$}", pb, width=w_use_bar))?;
                        }
                        if e_use_share {
                            let mut share_style = txt_style.clone();
                            share_style.set_fg(share_color);
                            cw.queue_g_string(&share_style, format!("{:>3.0}%", 100.0*stats.use_share()))?;
                        }
                        cw.queue_char(border_style, '│')?;
                    }
                    // free
                    cw.queue_g_string(txt_style, format!("{:>4}", file_size::fit_4(stats.available())))?;
                    cw.queue_char(border_style, '│')?;
                } else {
                    // size
                    cw.repeat(txt_style, &SPACE_FILLING, w_size)?;
                    cw.queue_char(border_style, '│')?;
                    // used
                    if e_use {
                        cw.repeat(txt_style, &SPACE_FILLING, wc_use)?;
                        cw.queue_char(border_style, '│')?;
                    }
                    // free
                    cw.repeat(txt_style, &SPACE_FILLING, w_free)?;
                    cw.queue_char(border_style, '│')?;
                }
                // mount point
                let s = &mount.info.mount_point.to_string_lossy();
                let matched_string = MatchedString::new(
                    self.filtered.as_ref().and_then(|f| f.pattern.search_string(s)),
                    s,
                    txt_style,
                    match_style,
                );
                matched_string.queue_on(&mut cw)?;
                idx += 1;
            }
            cw.fill(txt_style, &SPACE_FILLING)?;
            let scrollbar_style = if is_thumb(y, scrollbar) {
                &styles.scrollbar_thumb
            } else {
                &styles.scrollbar_track
            };
            scrollbar_style.queue_str(w, "▐")?;
        }
        Ok(())
    }

    fn on_internal(
        &mut self,
        w: &mut W,
        internal_exec: &InternalExecution,
        input_invocation: Option<&VerbInvocation>,
        trigger_type: TriggerType,
        cc: &CmdContext,
        screen: Screen,
    ) -> Result<AppStateCmdResult, ProgramError> {
        use Internal::*;
        Ok(match internal_exec.internal {
            Internal::back => {
                if let Some(f) = self.filtered.take() {
                    if !f.mounts.is_empty() {
                        self.selection_idx = self.mounts.iter()
                            .position(|m| m.info.id == f.mounts[f.selection_idx].info.id)
                            .unwrap(); // all filtered mounts come from self.mounts
                    }
                    AppStateCmdResult::Keep
                } else {
                    AppStateCmdResult::PopState
                }
            }
            Internal::line_down => {
                if let Some(f) = self.filtered.as_mut() {
                    if f.selection_idx + 1 < f.mounts.len() {
                        f.selection_idx += 1;
                    }
                } else {
                    if self.selection_idx + 1 < self.count() {
                        self.selection_idx += 1;
                    }
                }
                AppStateCmdResult::Keep
            }
            Internal::line_up => {
                if let Some(f) = self.filtered.as_mut() {
                    if f.selection_idx > 0 {
                        f.selection_idx -= 1;
                    }
                } else {
                    if self.selection_idx > 0 {
                        self.selection_idx -= 1;
                    }
                }
                AppStateCmdResult::Keep
            }
            Internal::open_stay => {
                let in_new_panel = input_invocation
                    .map(|inv| inv.bang)
                    .unwrap_or(internal_exec.bang);
                let dam = Dam::unlimited();
                let mut tree_options = self.tree_options();
                tree_options.show_root_fs = true;
                AppStateCmdResult::from_optional_state(
                    BrowserState::new(
                        self.selected_path().to_path_buf(),
                        tree_options,
                        screen,
                        &cc.con,
                        &dam,
                    ),
                    in_new_panel,
                )
            }
            Internal::panel_left => {
                if cc.areas.is_first() {
                    // we ask for the creation of a panel to the left
                    internal_focus::new_panel_on_path(
                        self.selected_path().to_path_buf(),
                        screen,
                        self.tree_options(),
                        PanelPurpose::None,
                        &cc.con,
                        HDir::Left,
                    )
                } else {
                    // we ask the app to focus the panel to the left
                    AppStateCmdResult::HandleInApp(Internal::panel_left)
                }
            }
            Internal::panel_right => {
                if cc.areas.is_last() {
                    // we ask for the creation of a panel to the right
                    internal_focus::new_panel_on_path(
                        self.selected_path().to_path_buf(),
                        screen,
                        self.tree_options(),
                        PanelPurpose::None,
                        &cc.con,
                        HDir::Right,
                    )
                } else {
                    // we ask the app to focus the panel to the right
                    AppStateCmdResult::HandleInApp(Internal::panel_right)
                }
            }
            Internal::page_down => {
                self.try_scroll(ScrollCommand::Pages(1));
                AppStateCmdResult::Keep
            }
            Internal::page_up => {
                self.try_scroll(ScrollCommand::Pages(-1));
                AppStateCmdResult::Keep
            }
            open_leave => AppStateCmdResult::PopStateAndReapply,
            _ => self.on_internal_generic(
                w,
                internal_exec,
                input_invocation,
                trigger_type,
                cc,
                screen,
            )?,
        })
    }

    fn on_click(
        &mut self,
        _x: u16,
        y: u16,
        _screen: Screen,
        _con: &AppContext,
    ) -> Result<AppStateCmdResult, ProgramError> {
        if y >= 2 {
            let y = y as usize - 2 + self.scroll;
            if y < self.mounts.len().into() {
                self.selection_idx = y;
            }
        }
        Ok(AppStateCmdResult::Keep)
    }
}

fn is_thumb(y: u16, scrollbar: Option<(u16, u16)>) -> bool {
    if let Some((sctop, scbottom)) = scrollbar {
        if sctop <= y && y <= scbottom {
            return true;
        }
    }
    false
}