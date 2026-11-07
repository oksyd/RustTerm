use super::*;
use gpui::{AnyElement, div, prelude::*, px, rgb};
use rustterm::database::TransactionStatus;
use rustterm::i18n::messages as i18n_key;

#[derive(Clone, Copy)]
enum TransactionAction {
    Begin,
    Commit,
    Rollback,
}

impl DatabaseView {
    fn transaction_action(&mut self, action: TransactionAction, cx: &mut Context<Self>) {
        if !self.available() {
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let generation = self.generation;
        self.busy = Some(i18n_key::DATABASE_UPDATING_TRANSACTION);
        self.error = None;
        let task = cx.background_executor().spawn(async move {
            match action {
                TransactionAction::Begin => client.begin().await,
                TransactionAction::Commit => client.commit().await,
                TransactionAction::Rollback => client.rollback().await,
            }
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.closed || this.generation != generation {
                    return;
                }
                this.busy = None;
                match result {
                    Ok(()) => {
                        if !matches!(action, TransactionAction::Begin) {
                            for document in &mut this.documents {
                                if let Some(table) = &mut document.table
                                    && !table.has_local_edit()
                                {
                                    table.invalidate();
                                    document.output = None;
                                    document.visible_rows.clear();
                                    document.value_view = None;
                                }
                            }
                            if let Some(table) = this.table_workspace() {
                                this.select_table_section(table.section, cx);
                            }
                        }
                    }
                    Err(error) => this.fail(error, cx),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_transaction(&self, cx: &Context<Self>) -> AnyElement {
        let state = self.client.as_ref().map_or(
            TransactionStatus::Unknown,
            DatabaseClient::transaction_status,
        );
        let label = match state {
            TransactionStatus::Idle => i18n_key::DATABASE_AUTO_COMMIT,
            TransactionStatus::Active => i18n_key::DATABASE_TRANSACTION_ACTIVE,
            TransactionStatus::Failed => i18n_key::DATABASE_TRANSACTION_FAILED_ROLLBACK_REQUIRED,
            TransactionStatus::Unknown => i18n_key::DATABASE_TRANSACTION_STATE_UNKNOWN,
        };
        let enabled = self.available();
        div()
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(rgb(match state {
                        TransactionStatus::Idle => ui::colors().muted,
                        TransactionStatus::Active => ui::colors().accent,
                        _ => ui::colors().warning,
                    }))
                    .child(tr(label)),
            )
            .when(state == TransactionStatus::Idle, |bar| {
                bar.child(
                    render::action(
                        "begin-transaction",
                        i18n_key::DATABASE_BEGIN,
                        enabled && state == TransactionStatus::Idle,
                        ui::ButtonStyle::Neutral,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transaction_action(TransactionAction::Begin, cx)
                    })),
                )
            })
            .when(state != TransactionStatus::Idle, |bar| {
                bar.child(
                    render::action(
                        "commit-transaction",
                        i18n_key::DATABASE_COMMIT,
                        enabled && state == TransactionStatus::Active,
                        ui::ButtonStyle::Neutral,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transaction_action(TransactionAction::Commit, cx)
                    })),
                )
            })
            .when(state != TransactionStatus::Idle, |bar| {
                bar.child(
                    render::action(
                        "rollback-transaction",
                        i18n_key::DATABASE_ROLLBACK,
                        enabled && state != TransactionStatus::Idle,
                        ui::ButtonStyle::Neutral,
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.transaction_action(TransactionAction::Rollback, cx)
                    })),
                )
            })
            .into_any_element()
    }
}
