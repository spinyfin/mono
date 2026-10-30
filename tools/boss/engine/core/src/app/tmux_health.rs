use super::*;

impl ServerState {
    pub(super) async fn start_tmux_health_monitor(self: &Arc<Self>) {
        let mut changes = crate::tmux_health::subscribe();
        let mut servers = Vec::<boss_tmux::Tmux>::new();
        self.refresh_tmux_health(&mut servers).await;
        let weak = Arc::downgrade(self);
        let socket_path = self.tmux_socket_path.clone();
        boss_task_scope::spawn(async move {
            // Also notice executable upgrades or external server replacements
            // while no worker is spawning. Lifecycle notifications refresh sooner.
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    observation = changes.recv() => {
                        if let Ok(tmux) = observation {
                            if tmux.socket_path().is_some_and(|path| path != socket_path) { continue; }
                            servers.retain(|old| old.socket_path() != tmux.socket_path());
                            servers.push(tmux);
                        }
                    }
                    _ = interval.tick() => {}
                }
                let Some(state) = weak.upgrade() else { return };
                state.refresh_tmux_health(&mut servers).await;
            }
        });
    }

    async fn refresh_tmux_health(self: &Arc<Self>, servers: &mut Vec<boss_tmux::Tmux>) {
        let mut issues = Vec::new();
        match boss_tmux::Tmux::resolve(&self.tmux_socket_path) {
            Ok(tmux) => {
                issues.extend(crate::tmux_health::installed_warning(&tmux).await);
                servers.retain(|old| old.socket_path() != tmux.socket_path());
                servers.push(tmux);
            }
            Err(error) => issues.extend(crate::tmux_health::version_warning(
                "Installed tmux",
                &error.to_string(),
            )),
        }
        for server in servers.iter() {
            issues.extend(crate::tmux_health::server_warning(server).await);
        }
        // The app identifies issues by kind. Keep one banner entry while
        // retaining evidence from both the installed executable and live servers.
        let issues: Vec<_> = issues
            .into_iter()
            .reduce(|mut combined, issue| {
                combined.body.push_str("\n\n");
                combined.body.push_str(&issue.body);
                combined
            })
            .into_iter()
            .collect();
        let changed = {
            let mut current = self.tmux_health_issues.write().expect("tmux health lock poisoned");
            if *current == issues {
                false
            } else {
                *current = issues;
                true
            }
        };
        if changed {
            self.broadcast_engine_health().await;
        }
    }
}
