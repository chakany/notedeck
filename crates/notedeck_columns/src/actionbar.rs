use crate::{
    column::Columns,
    nav::{RouterAction, RouterType},
    route::Route,
    timeline::{ThreadSelection, TimelineCache, TimelineKind},
};

use enostr::{Pubkey, RelayPool};
use nostrdb::{Ndb, NoteKey, Transaction};
use notedeck::{
    get_wallet_for_mut, note::ZapTargetAmount, Accounts, GlobalWallet, Images, NoteAction,
    NoteCache, NoteZapTargetOwned, UnknownIds, ZapAction, ZapTarget, ZappingError, Zaps,
};
use tracing::error;

pub struct NewNotes {
    pub id: TimelineKind,
    pub notes: Vec<NoteKey>,
}

pub enum TimelineOpenResult {
    NewNotes(NewNotes),
}

struct NoteActionResponse {
    timeline_res: Option<TimelineOpenResult>,
    router_action: Option<RouterAction>,
}

/// The note action executor for notedeck_columns
#[allow(clippy::too_many_arguments)]
fn execute_note_action(
    action: NoteAction,
    ndb: &Ndb,
    timeline_cache: &mut TimelineCache,
    note_cache: &mut NoteCache,
    pool: &mut RelayPool,
    txn: &Transaction,
    accounts: &mut Accounts,
    global_wallet: &mut GlobalWallet,
    zaps: &mut Zaps,
    images: &mut Images,
    router_type: RouterType,
    ui: &mut egui::Ui,
) -> NoteActionResponse {
    let mut timeline_res = None;
    let mut router_action = None;

    match action {
        NoteAction::Reply(note_id) => {
            router_action = Some(RouterAction::route_to(Route::reply(note_id)));
        }
        NoteAction::Profile(pubkey) => {
            let kind = TimelineKind::Profile(pubkey);
            router_action = Some(RouterAction::route_to(Route::Timeline(kind.clone())));
            timeline_res = timeline_cache.open(ndb, note_cache, txn, pool, &kind);
        }
        NoteAction::Note(note_id) => 'ex: {
            let Ok(thread_selection) = ThreadSelection::from_note_id(ndb, note_cache, txn, note_id)
            else {
                tracing::error!("No thread selection for {}?", hex::encode(note_id.bytes()));
                break 'ex;
            };

            let kind = TimelineKind::Thread(thread_selection);
            router_action = Some(RouterAction::route_to(Route::Timeline(kind.clone())));
            // NOTE!!: you need the note_id to timeline root id thing

            timeline_res = timeline_cache.open(ndb, note_cache, txn, pool, &kind);
        }
        NoteAction::Hashtag(htag) => {
            let kind = TimelineKind::Hashtag(htag.clone());
            router_action = Some(RouterAction::route_to(Route::Timeline(kind.clone())));
            timeline_res = timeline_cache.open(ndb, note_cache, txn, pool, &kind);
        }
        NoteAction::Quote(note_id) => {
            router_action = Some(RouterAction::route_to(Route::quote(note_id)));
        }
        NoteAction::Zap(zap_action) => 's: {
            let Some(cur_acc) = accounts.get_selected_account_mut() else {
                break 's;
            };

            let sender = cur_acc.key.pubkey;

            match &zap_action {
                ZapAction::Send(target) => 'a: {
                    let Some(wallet) = get_wallet_for_mut(accounts, global_wallet, sender.bytes())
                    else {
                        zaps.send_error(
                            sender.bytes(),
                            ZapTarget::Note((&target.target).into()),
                            ZappingError::SenderNoWallet,
                        );
                        break 'a;
                    };

                    if let RouterType::Sheet = router_type {
                        router_action = Some(RouterAction::GoBack);
                    }

                    send_zap(
                        &sender,
                        zaps,
                        pool,
                        target,
                        wallet.default_zap.get_default_zap_msats(),
                    )
                }
                ZapAction::ClearError(target) => clear_zap_error(&sender, zaps, target),
                ZapAction::CustomizeAmount(target) => {
                    let route = Route::CustomizeZapAmount(target.to_owned());
                    router_action = Some(RouterAction::route_to_sheet(route));
                }
            }
        }
        NoteAction::Context(context) => match ndb.get_note_by_key(txn, context.note_key) {
            Err(err) => tracing::error!("{err}"),
            Ok(note) => {
                context.action.process(ui, &note, pool);
            }
        },
        NoteAction::Media(media_action) => {
            media_action.process(images);
        }
    }

    NoteActionResponse {
        timeline_res,
        router_action,
    }
}

/// Execute a NoteAction and process the result
#[allow(clippy::too_many_arguments)]
pub fn execute_and_process_note_action(
    action: NoteAction,
    ndb: &Ndb,
    columns: &mut Columns,
    col: usize,
    timeline_cache: &mut TimelineCache,
    note_cache: &mut NoteCache,
    pool: &mut RelayPool,
    txn: &Transaction,
    unknown_ids: &mut UnknownIds,
    accounts: &mut Accounts,
    global_wallet: &mut GlobalWallet,
    zaps: &mut Zaps,
    images: &mut Images,
    ui: &mut egui::Ui,
) -> Option<RouterAction> {
    let router_type = {
        let sheet_router = &mut columns.column_mut(col).sheet_router;

        if sheet_router.route().is_some() {
            RouterType::Sheet
        } else {
            RouterType::Stack
        }
    };

    let resp = execute_note_action(
        action,
        ndb,
        timeline_cache,
        note_cache,
        pool,
        txn,
        accounts,
        global_wallet,
        zaps,
        images,
        router_type,
        ui,
    );

    if let Some(br) = resp.timeline_res {
        br.process(ndb, note_cache, txn, timeline_cache, unknown_ids);
    }

    resp.router_action
}

fn send_zap(
    sender: &Pubkey,
    zaps: &mut Zaps,
    pool: &RelayPool,
    target_amount: &ZapTargetAmount,
    default_msats: u64,
) {
    let zap_target = ZapTarget::Note((&target_amount.target).into());

    let msats = target_amount.specified_msats.unwrap_or(default_msats);

    let sender_relays: Vec<String> = pool.relays.iter().map(|r| r.url().to_string()).collect();
    zaps.send_zap(sender.bytes(), sender_relays, zap_target, msats);
}

fn clear_zap_error(sender: &Pubkey, zaps: &mut Zaps, target: &NoteZapTargetOwned) {
    zaps.clear_error_for(sender.bytes(), ZapTarget::Note(target.into()));
}

impl TimelineOpenResult {
    pub fn new_notes(notes: Vec<NoteKey>, id: TimelineKind) -> Self {
        Self::NewNotes(NewNotes::new(notes, id))
    }

    pub fn process(
        &self,
        ndb: &Ndb,
        note_cache: &mut NoteCache,
        txn: &Transaction,
        storage: &mut TimelineCache,
        unknown_ids: &mut UnknownIds,
    ) {
        match self {
            // update the thread for next render if we have new notes
            TimelineOpenResult::NewNotes(new_notes) => {
                new_notes.process(storage, ndb, txn, unknown_ids, note_cache);
            }
        }
    }
}

impl NewNotes {
    pub fn new(notes: Vec<NoteKey>, id: TimelineKind) -> Self {
        NewNotes { notes, id }
    }

    /// Simple helper for processing a NewThreadNotes result. It simply
    /// inserts/merges the notes into the corresponding timeline cache
    pub fn process(
        &self,
        timeline_cache: &mut TimelineCache,
        ndb: &Ndb,
        txn: &Transaction,
        unknown_ids: &mut UnknownIds,
        note_cache: &mut NoteCache,
    ) {
        let reversed = matches!(&self.id, TimelineKind::Thread(_));

        let timeline = if let Some(profile) = timeline_cache.timelines.get_mut(&self.id) {
            profile
        } else {
            error!("NewNotes: could not get timeline for key {}", self.id);
            return;
        };

        if let Err(err) = timeline.insert(&self.notes, ndb, txn, unknown_ids, note_cache, reversed)
        {
            error!("error inserting notes into profile timeline: {err}")
        }
    }
}
