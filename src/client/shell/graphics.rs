use super::*;

impl ClientShellState {
    #[cfg(unix)]
    pub(crate) fn graphics_scope(&self) -> &str {
        self.graphics.scope()
    }

    #[cfg(unix)]
    pub(crate) fn trust_direct_graphics_asset(
        &mut self,
        key: &crate::protocol::SurfaceGraphicsAssetKey,
        image_id: u32,
    ) -> bool {
        self.graphics.trust_direct_asset(key, image_id)
    }

    #[cfg(unix)]
    pub(crate) fn retire_direct_graphics_image(&mut self, image_id: u32) {
        self.graphics.retire_direct_image(image_id);
    }

    pub(crate) fn take_pending_graphics_cleanup(&mut self) -> Vec<u8> {
        self.graphics.take_pending_cleanup()
    }

    pub(crate) fn set_host_graphics_protocol(
        &mut self,
        protocol: crate::kitty_graphics::sixel::HostGraphicsProtocol,
    ) {
        if self.host_graphics_protocol == protocol {
            return;
        }
        self.host_graphics_protocol = protocol;
        self.graphics.set_protocol(protocol);
    }

    pub(crate) fn set_graphics_cell_size(&mut self, width_px: u32, height_px: u32) {
        self.graphics_cell_size = crate::kitty_graphics::HostCellSize {
            width_px: width_px.max(1),
            height_px: height_px.max(1),
        };
    }

    pub(super) fn compose_graphics(&mut self, frame: &mut FrameData, layout: ClientShellLayout) {
        let local_cover = self.overlay.is_some()
            || self.mode != ClientShellMode::Terminal
            || self.endpoint_error.is_some()
            || self.config_diagnostic.is_some()
            || self.visible_endpoint_notice.is_some()
            || self.visible_notification.is_some()
            || self.copy_feedback.is_some()
            || self
                .selection
                .as_ref()
                .is_some_and(|selection| selection.is_visible());
        let visibility = if local_cover {
            crate::kitty_graphics::surface::Visibility::Hidden
        } else if self.hits.popup.is_some() {
            crate::kitty_graphics::surface::Visibility::Popup
        } else {
            crate::kitty_graphics::surface::Visibility::Main
        };
        let popup_origin = self
            .hits
            .popup
            .as_ref()
            .map(|popup| (popup.inner_rect.x, popup.inner_rect.y));
        // SIXEL output scrolls the host when it reaches the last terminal row,
        // so keep that row free.
        let safe_bottom = layout
            .pane_surface
            .y
            .saturating_add(layout.pane_surface.height)
            .checked_sub(1);
        frame.graphics = self.graphics.encode(
            visibility,
            (layout.pane_surface.x, layout.pane_surface.y),
            popup_origin,
            self.graphics_cell_size,
            safe_bottom,
        );
    }
}
