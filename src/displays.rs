//! The guest's entities as block displays on the server, and the rule that none of them outlives its
//! session.
//!
//! A display the plugin spawned is an ordinary entity of the server. It is saved with its chunk, so when
//! the chunk unloads and loads again (or the server stops and starts, or is killed after a save) the
//! world holds a NEW entity the plugin has never heard of, while the plugin's handle points at the dead
//! one: the poses move nothing and `remove` removes nothing. So every display carries a tag in the
//! entity's own persistent data, `pumpkin-link` / `guest` = `<session>:<guest entity>`, and a periodic
//! sweep looks at the tagged entities the world holds:
//!
//! * the tag names the current session and a guest entity the plugin shows, and the plugin's handle is
//!   that very entity: nothing to do;
//! * the same, but the plugin's handle is gone from the world (its chunk was unloaded) and this is the
//!   reloaded copy: it becomes the handle, and the last pose is applied to it again;
//! * the same, but the plugin's handle is alive too: this one is a duplicate, removed;
//! * any other tag (an earlier session, an earlier run of the server, a guest that is not shown):
//!   removed.
//!
//! An entity without our tag is never looked at again, so another plugin's or a map maker's block
//! displays are safe. The server-facing half (`DisplayWorld`) is a trait so all of this is tested on
//! any target with a fake world.

use std::collections::HashMap;

/// The namespace and key of the tag in the entity's persistent data.
pub const NAMESPACE: &str = "pumpkin-link";
pub const KEY: &str = "guest";

/// What the tag says: which session of this plugin showed the entity, and for which guest entity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub session: String,
    pub guest: u32,
}

impl Tag {
    pub fn encode(&self) -> String {
        format!("{}:{}", self.session, self.guest)
    }

    /// `None` for anything that is not our format; such an entity is left alone.
    pub fn parse(text: &str) -> Option<Self> {
        let (session, guest) = text.rsplit_once(':')?;
        if session.is_empty() {
            return None;
        }
        Some(Self {
            session: session.to_owned(),
            guest: guest.parse().ok()?,
        })
    }
}

/// An entity of the world that carries our tag.
pub struct Found<H> {
    pub handle: H,
    pub server_id: u32,
    pub tag: Tag,
}

/// The server's side: spawn, move and remove a display, and list the ones that carry our tag.
pub trait DisplayWorld {
    type Handle;
    /// Spawns a display carrying `tag`; `None` when it cannot be shown.
    fn create(&mut self, tag: &Tag) -> Option<(Self::Handle, u32)>;
    fn place(&mut self, handle: &Self::Handle, pos: [f64; 3], yaw: f32, pitch: f32);
    fn remove(&mut self, handle: &Self::Handle);
    /// Every entity of every loaded chunk that carries a tag of ours (an entity without one is not listed).
    fn scan(&mut self) -> Vec<Found<Self::Handle>>;
}

struct Shown<H> {
    handle: H,
    server_id: u32,
    last: Option<([f64; 3], f32, f32)>,
    /// Sweeps in a row that did not list the handle's entity: its chunk is unloaded, or it is gone.
    missed: u32,
    /// Times in a row the display was made again and the world has not listed it since (the guest stays where
    /// no chunk is loaded); each one doubles the misses before the next try.
    remade: u32,
}

/// A display the world is not listing for this many sweeps in a row is taken as gone (one miss can be a
/// sweep that ran before the world listed a display just spawned).
const GONE_AFTER_MISSES: u32 = 2;

/// The misses before a display is made again double with each try that the world did not list, up to
/// `GONE_AFTER_MISSES << MAX_BACKOFF_DOUBLINGS` sweeps (64, about a minute while a link runs).
const MAX_BACKOFF_DOUBLINGS: u32 = 5;

fn misses_before_remaking(remade: u32) -> u32 {
    GONE_AFTER_MISSES << remade.min(MAX_BACKOFF_DOUBLINGS)
}

pub struct Displays<W: DisplayWorld> {
    world: W,
    /// Unique per run of the server, so a tag written by an earlier run is never the current session's.
    run: String,
    /// Bumped each time the guest's entities are cleared, so one session's tags are not the next one's.
    epoch: u32,
    shown: HashMap<u32, Shown<W::Handle>>,
    /// Guest entities whose spawn could not be shown yet (the server had no world), with the last pose sent for them.
    wanted: HashMap<u32, Option<([f64; 3], f32, f32)>>,
}

impl<W: DisplayWorld> Displays<W> {
    pub fn new(world: W, run: String) -> Self {
        Self {
            world,
            run,
            epoch: 0,
            shown: HashMap::new(),
            wanted: HashMap::new(),
        }
    }

    pub fn world(&mut self) -> &mut W {
        &mut self.world
    }

    fn session(&self) -> String {
        format!("{}-{}", self.run, self.epoch)
    }

    pub fn len(&self) -> usize {
        self.shown.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shown.is_empty()
    }

    pub fn is_shown(&self, guest: u32) -> bool {
        self.shown.contains_key(&guest)
    }

    /// Shows the guest's entity; when it cannot be shown yet (no world) it is kept and retried by the next sweep.
    pub fn spawn(&mut self, guest: u32) -> bool {
        self.despawn(guest);
        match self.create(guest) {
            Some((handle, server_id)) => {
                self.shown.insert(
                    guest,
                    Shown {
                        handle,
                        server_id,
                        last: None,
                        missed: 0,
                        remade: 0,
                    },
                );
                true
            }
            None => {
                self.wanted.insert(guest, None);
                false
            }
        }
    }

    fn create(&mut self, guest: u32) -> Option<(W::Handle, u32)> {
        let tag = Tag {
            session: self.session(),
            guest,
        };
        self.world.create(&tag)
    }

    pub fn pose(&mut self, guest: u32, pos: [f64; 3], yaw: f32, pitch: f32) {
        if let Some(w) = self.wanted.get_mut(&guest) {
            *w = Some((pos, yaw, pitch));
            return;
        }
        let gone = self
            .shown
            .get(&guest)
            .is_some_and(|s| s.missed >= misses_before_remaking(s.remade));
        if gone && let Some((handle, server_id)) = self.create(guest) {
            // Its chunk has been unloaded since and nothing came back: show it again here, where the guest is
            // now. If the old chunk loads later, its copy is a duplicate of a live display and the sweep removes it.
            if let Some(s) = self.shown.get_mut(&guest) {
                self.world.remove(&s.handle);
                s.handle = handle;
                s.server_id = server_id;
                s.missed = 0;
                s.remade += 1;
            }
        }
        if let Some(s) = self.shown.get_mut(&guest) {
            self.world.place(&s.handle, pos, yaw, pitch);
            s.last = Some((pos, yaw, pitch));
        }
    }

    pub fn despawn(&mut self, guest: u32) {
        self.wanted.remove(&guest);
        if let Some(s) = self.shown.remove(&guest) {
            self.world.remove(&s.handle);
        }
    }

    /// The session is over: removes every display, the reloaded copies too, and starts a new session.
    pub fn clear(&mut self) {
        self.wanted.clear();
        for (_, s) in self.shown.drain() {
            self.world.remove(&s.handle);
        }
        self.epoch += 1;
        self.sweep();
    }

    /// Retries the spawns that could not be shown, then looks at the tagged entities of the world (see the
    /// module's rules); returns how many it removed or adopted.
    pub fn sweep(&mut self) -> usize {
        let wanted: Vec<u32> = self.wanted.keys().copied().collect();
        for guest in wanted {
            let Some((handle, server_id)) = self.create(guest) else {
                break; // still no world
            };
            let last = self.wanted.remove(&guest).flatten();
            if let Some((pos, yaw, pitch)) = last {
                self.world.place(&handle, pos, yaw, pitch);
            }
            self.shown.insert(
                guest,
                Shown {
                    handle,
                    server_id,
                    last,
                    missed: 0,
                    remade: 0,
                },
            );
        }
        let found = self.world.scan();
        let alive: Vec<u32> = found.iter().map(|f| f.server_id).collect();
        let session = self.session();
        let mut changed = 0;
        for f in found {
            let ours = f.tag.session == session;
            let Some(s) = (if ours {
                self.shown.get_mut(&f.tag.guest)
            } else {
                None
            }) else {
                self.world.remove(&f.handle);
                changed += 1;
                continue;
            };
            if s.server_id == f.server_id {
                continue;
            }
            if alive.contains(&s.server_id) {
                self.world.remove(&f.handle);
            } else {
                s.handle = f.handle;
                s.server_id = f.server_id;
                s.missed = 0;
                if let Some((pos, yaw, pitch)) = s.last {
                    self.world.place(&s.handle, pos, yaw, pitch);
                }
            }
            changed += 1;
        }
        for s in self.shown.values_mut() {
            if alive.contains(&s.server_id) {
                s.missed = 0;
                s.remade = 0;
            } else {
                s.missed += 1;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world of entities with ids, raw tags and positions; `unload` and `reload` model a chunk going to
    /// disk and coming back as new entities.
    #[derive(Default)]
    struct Fake {
        next_id: u32,
        loaded: Vec<E>,
        disk: Vec<E>,
        /// Displays made since the start.
        created: usize,
        /// New displays land where no chunk is loaded: the world never lists them.
        create_unloaded: bool,
    }

    #[derive(Clone, Debug)]
    struct E {
        id: u32,
        tag: Option<String>,
        pos: [f64; 3],
    }

    impl Fake {
        /// An entity somebody else made (another plugin, a map maker): no tag of ours.
        fn foreign(&mut self, tag: Option<&str>) -> u32 {
            self.next_id += 1;
            self.loaded.push(E {
                id: self.next_id,
                tag: tag.map(str::to_owned),
                pos: [0.0; 3],
            });
            self.next_id
        }
        fn unload(&mut self) {
            self.disk.append(&mut self.loaded);
        }
        fn reload(&mut self) {
            for mut e in std::mem::take(&mut self.disk) {
                self.next_id += 1;
                e.id = self.next_id;
                self.loaded.push(e);
            }
        }
        fn count(&self) -> usize {
            self.loaded.len() + self.disk.len()
        }
        fn at(&self, id: u32) -> Option<&E> {
            self.loaded.iter().find(|e| e.id == id)
        }
    }

    impl DisplayWorld for Fake {
        type Handle = u32;
        fn create(&mut self, tag: &Tag) -> Option<(u32, u32)> {
            self.next_id += 1;
            self.created += 1;
            let e = E {
                id: self.next_id,
                tag: Some(tag.encode()),
                pos: [0.0; 3],
            };
            if self.create_unloaded {
                self.disk.push(e);
            } else {
                self.loaded.push(e);
            }
            Some((self.next_id, self.next_id))
        }
        fn place(&mut self, h: &u32, pos: [f64; 3], _: f32, _: f32) {
            if let Some(e) = self.loaded.iter_mut().find(|e| e.id == *h) {
                e.pos = pos;
            }
        }
        fn remove(&mut self, h: &u32) {
            self.loaded.retain(|e| e.id != *h);
        }
        fn scan(&mut self) -> Vec<Found<u32>> {
            self.loaded
                .iter()
                .filter_map(|e| {
                    Some(Found {
                        handle: e.id,
                        server_id: e.id,
                        tag: Tag::parse(e.tag.as_deref()?)?,
                    })
                })
                .collect()
        }
    }

    fn displays() -> Displays<Fake> {
        Displays::new(Fake::default(), "run1".into())
    }

    #[test]
    fn tag_round_trip_and_garbage() {
        let t = Tag {
            session: "a1b2-3".into(),
            guest: 90,
        };
        assert_eq!(Tag::parse(&t.encode()), Some(t));
        for bad in ["", "x", "x:", ":4", "x:y", "x:-1"] {
            assert_eq!(Tag::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn spawn_pose_despawn() {
        let mut d = displays();
        assert!(d.spawn(90));
        d.pose(90, [1.0, 2.0, 3.0], 0.0, 0.0);
        assert_eq!(d.world().loaded[0].pos, [1.0, 2.0, 3.0]);
        assert_eq!(
            d.sweep(),
            0,
            "nothing to do while everything is as it should be"
        );
        d.despawn(90);
        assert_eq!(d.world().count(), 0);
    }

    #[test]
    fn a_second_spawn_of_the_same_guest_replaces_the_first() {
        let mut d = displays();
        d.spawn(90);
        d.spawn(90);
        assert_eq!(d.world().count(), 1);
    }

    #[test]
    fn a_reloaded_display_is_adopted_and_follows_the_poses_again() {
        let mut d = displays();
        d.spawn(90);
        d.pose(90, [5.0, 6.0, 7.0], 1.0, 2.0);
        // the chunk goes to disk and comes back: a new entity, the plugin's handle is dead
        d.world().unload();
        d.world().reload();
        let before = d.world().loaded[0].id;
        // without the sweep a pose reaches nothing
        d.pose(90, [9.0, 9.0, 9.0], 0.0, 0.0);
        assert_eq!(
            d.world().at(before).unwrap().pos,
            [5.0, 6.0, 7.0],
            "the stale handle moved nothing"
        );
        assert_eq!(d.sweep(), 1);
        assert_eq!(d.world().count(), 1, "adopted, not duplicated");
        assert_eq!(
            d.world().at(before).unwrap().pos,
            [9.0, 9.0, 9.0],
            "the last pose is applied to it at once"
        );
        d.pose(90, [1.0, 1.0, 1.0], 0.0, 0.0);
        assert_eq!(d.world().at(before).unwrap().pos, [1.0, 1.0, 1.0]);
        d.despawn(90);
        assert_eq!(d.world().count(), 0, "and it is the one that is removed");
    }

    #[test]
    fn clear_removes_the_reloaded_copy_too() {
        // the failing case of the live test: stop the link after the chunk has been away
        let mut d = displays();
        d.spawn(90);
        d.world().unload();
        d.world().reload();
        d.clear();
        assert_eq!(d.world().count(), 0);
    }

    #[test]
    fn a_duplicate_of_a_live_display_is_removed() {
        let mut d = displays();
        d.spawn(90);
        let tag = Tag {
            session: d.session(),
            guest: 90,
        }
        .encode();
        d.world().foreign(Some(&tag));
        assert_eq!(d.sweep(), 1);
        assert_eq!(d.world().count(), 1);
    }

    #[test]
    fn leftovers_of_an_earlier_run_or_session_are_removed() {
        // a kill -9 after a save: the new run knows nothing, the world still holds the display
        let mut old = displays();
        old.spawn(90);
        let mut fake = std::mem::take(old.world());
        fake.unload();
        fake.reload();
        let mut d = Displays::new(fake, "run2".into());
        assert_eq!(d.sweep(), 1);
        assert_eq!(d.world().count(), 0);
        // an earlier session of the same run
        d.spawn(1);
        let stale = Tag {
            session: "run2-0".into(),
            guest: 77,
        }
        .encode();
        d.world().foreign(Some(&stale));
        d.clear();
        assert_eq!(d.world().count(), 0);
        d.spawn(1);
        let old_epoch = Tag {
            session: "run2-0".into(),
            guest: 1,
        }
        .encode();
        d.world().foreign(Some(&old_epoch));
        assert_eq!(
            d.sweep(),
            1,
            "the tag of the previous epoch is not this session's"
        );
        assert_eq!(d.world().count(), 1);
    }

    #[test]
    fn a_tag_for_a_guest_that_is_not_shown_is_removed() {
        let mut d = displays();
        d.spawn(1);
        let t = Tag {
            session: d.session(),
            guest: 2,
        }
        .encode();
        d.world().foreign(Some(&t));
        assert_eq!(d.sweep(), 1);
        assert_eq!(d.world().count(), 1);
    }

    #[test]
    fn entities_that_are_not_ours_are_never_touched() {
        let mut d = displays();
        // a map maker's display (no tag at all), another plugin's data in another namespace (modelled as
        // a tag that is not our format), and a tag that merely looks like ours but is not parseable
        let a = d.world().foreign(None);
        let b = d.world().foreign(Some("other-plugin:data"));
        let c = d.world().foreign(Some("run1-0:not-a-number"));
        let e = d.world().foreign(Some(""));
        d.spawn(90);
        for _ in 0..3 {
            d.sweep();
        }
        d.world().unload();
        d.world().reload();
        d.sweep();
        d.clear();
        d.spawn(91);
        d.clear();
        // the four are still there (as new entities after the reload)
        assert_eq!(d.world().count(), 4, "{:?}", (a, b, c, e));
        assert!(d.world().loaded.iter().any(|x| x.tag.is_none()));
        assert!(
            d.world()
                .loaded
                .iter()
                .any(|x| x.tag.as_deref() == Some("other-plugin:data"))
        );
    }

    /// A world whose `create` fails until it is told the world exists (the server had not loaded its worlds yet).
    #[derive(Default)]
    struct Late {
        inner: Fake,
        ready: bool,
    }
    impl DisplayWorld for Late {
        type Handle = u32;
        fn create(&mut self, tag: &Tag) -> Option<(u32, u32)> {
            if self.ready {
                self.inner.create(tag)
            } else {
                None
            }
        }
        fn place(&mut self, h: &u32, pos: [f64; 3], yaw: f32, pitch: f32) {
            self.inner.place(h, pos, yaw, pitch)
        }
        fn remove(&mut self, h: &u32) {
            self.inner.remove(h)
        }
        fn scan(&mut self) -> Vec<Found<u32>> {
            self.inner.scan()
        }
    }

    #[test]
    fn a_spawn_that_could_not_be_shown_is_retried_once_the_world_exists() {
        // auto_start: the guest's spawn arrives before the first sweep has found a world
        let mut d = Displays::new(Late::default(), "run1".into());
        assert!(!d.spawn(90));
        d.pose(90, [4.0, 5.0, 6.0], 0.0, 0.0); // poses for it are kept, not dropped
        assert_eq!(d.sweep(), 0);
        assert_eq!(d.world().inner.count(), 0, "still no world");
        d.world().ready = true;
        d.sweep();
        assert_eq!(d.world().inner.count(), 1);
        assert_eq!(
            d.world().inner.loaded[0].pos,
            [4.0, 5.0, 6.0],
            "shown at the last pose it was sent"
        );
        assert!(d.is_shown(90));
        // a despawn in the meantime cancels the retry
        let mut d = Displays::new(Late::default(), "run1".into());
        d.spawn(7);
        d.despawn(7);
        d.world().ready = true;
        d.sweep();
        assert_eq!(d.world().inner.count(), 0);
    }

    #[test]
    fn a_display_whose_chunk_stays_unloaded_is_shown_again_at_the_next_pose() {
        let mut d = displays();
        d.spawn(90);
        d.pose(90, [5.0, 6.0, 7.0], 0.0, 0.0);
        d.world().unload(); // and no reload: the guest's place stays unloaded
        d.sweep();
        d.sweep();
        d.sweep();
        // the guest walks to where the host's player is: a pose arrives
        d.pose(90, [1.0, 2.0, 3.0], 0.0, 0.0);
        assert_eq!(d.world().loaded.len(), 1, "a new display exists");
        assert_eq!(d.world().loaded[0].pos, [1.0, 2.0, 3.0]);
        // later the old chunk loads again: the old copy is a duplicate of a live display and goes
        d.world().reload();
        assert_eq!(d.world().count(), 2);
        d.sweep();
        assert_eq!(d.world().count(), 1, "the reloaded old copy was removed");
        assert_eq!(d.world().loaded[0].pos, [1.0, 2.0, 3.0]);
        d.pose(90, [9.0, 9.0, 9.0], 0.0, 0.0);
        assert_eq!(d.world().loaded[0].pos, [9.0, 9.0, 9.0]);
    }

    #[test]
    fn a_display_made_again_where_no_chunk_is_loaded_is_tried_less_and_less_often() {
        let mut d = displays();
        d.spawn(90);
        d.pose(90, [5.0, 6.0, 7.0], 0.0, 0.0);
        d.world().unload();
        // the guest stays far from every player: whatever is made for it is never listed
        d.world().create_unloaded = true;
        let before = d.world().created;
        for _ in 0..64 {
            d.sweep();
            d.pose(90, [1000.0, 64.0, 1000.0], 0.0, 0.0);
        }
        // made again after 2, then 4, 8, 16 and 32 more missed sweeps: 5 times in 64 sweeps, not 32
        assert_eq!(d.world().created - before, 5);
        // once the world lists it again, the wait is back to two sweeps
        d.world().create_unloaded = false;
        d.world().reload();
        d.sweep();
        assert!(d.is_shown(90));
        let listed = d.world().created;
        d.world().unload();
        d.sweep();
        d.sweep();
        d.pose(90, [1.0, 2.0, 3.0], 0.0, 0.0);
        assert_eq!(d.world().created, listed + 1);
    }

    #[test]
    fn a_display_is_shown_again_after_exactly_two_missed_sweeps() {
        let mut d = displays();
        d.spawn(90);
        d.pose(90, [5.0, 6.0, 7.0], 0.0, 0.0);
        d.world().unload(); // and no reload
        d.sweep(); // the first sweep that does not list it
        d.pose(90, [1.0, 2.0, 3.0], 0.0, 0.0);
        assert_eq!(
            d.world().loaded.len(),
            0,
            "after one missed sweep no new display is made: that sweep may only have run too early"
        );
        d.sweep(); // the second
        d.pose(90, [4.0, 5.0, 6.0], 0.0, 0.0);
        assert_eq!(
            d.world().loaded.len(),
            1,
            "after exactly two missed sweeps it is shown again"
        );
        assert_eq!(d.world().loaded[0].pos, [4.0, 5.0, 6.0]);
        // the old chunk loads later: its copy is removed
        d.world().reload();
        assert_eq!(d.world().count(), 2);
        d.sweep();
        assert_eq!(d.world().count(), 1, "the reloaded old copy was removed");
    }

    #[test]
    fn one_missed_sweep_does_not_recreate_a_display() {
        // the sweep may run before the world lists a display that was just spawned: no second display
        let mut d = displays();
        d.spawn(90);
        let id = d.world().loaded[0].id;
        let hidden = d.world().loaded.remove(0);
        d.sweep(); // not listed this time
        d.world().loaded.push(hidden);
        d.pose(90, [1.0, 1.0, 1.0], 0.0, 0.0);
        assert_eq!(d.world().count(), 1);
        assert_eq!(d.world().at(id).unwrap().pos, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn misses_that_are_not_in_a_row_never_add_up() {
        // listed again in between: the count starts over, so two separate misses are not "gone"
        let mut d = displays();
        d.spawn(90);
        let id = d.world().loaded[0].id;
        let hidden = d.world().loaded.remove(0);
        d.sweep(); // a miss
        d.world().loaded.push(hidden);
        d.sweep(); // listed: the count starts over
        let hidden = d.world().loaded.remove(0);
        d.sweep(); // one miss again, not two in a row
        d.world().loaded.push(hidden);
        d.pose(90, [1.0, 1.0, 1.0], 0.0, 0.0);
        assert_eq!(d.world().count(), 1, "no second display");
        assert_eq!(d.world().at(id).unwrap().pos, [1.0, 1.0, 1.0]);
    }

    #[test]
    fn a_display_shown_again_is_not_made_again_by_every_pose_that_follows() {
        let mut d = displays();
        d.spawn(90);
        d.world().unload(); // and no reload
        d.sweep();
        d.sweep();
        d.pose(90, [1.0, 2.0, 3.0], 0.0, 0.0); // shown again here
        assert_eq!(d.world().loaded.len(), 1);
        let id = d.world().loaded[0].id;
        d.pose(90, [4.0, 5.0, 6.0], 0.0, 0.0);
        d.pose(90, [7.0, 8.0, 9.0], 0.0, 0.0);
        assert_eq!(d.world().loaded.len(), 1, "still the one display");
        assert_eq!(d.world().loaded[0].id, id, "the same one, followed");
        assert_eq!(d.world().loaded[0].pos, [7.0, 8.0, 9.0]);
    }

    #[test]
    fn a_clear_forgets_the_spawns_that_were_waiting_for_a_world() {
        let mut d = Displays::new(Late::default(), "run1".into());
        assert!(!d.spawn(90));
        d.pose(90, [4.0, 5.0, 6.0], 0.0, 0.0);
        d.clear(); // the session is over while there is still no world
        d.world().ready = true;
        d.sweep();
        assert_eq!(
            d.world().inner.count(),
            0,
            "nothing from the old session shows up later"
        );
        assert!(!d.is_shown(90));
    }

    #[test]
    fn the_tag_names_the_session_and_a_clear_starts_the_next_one() {
        let mut d = displays();
        d.spawn(1);
        assert_eq!(d.world().loaded[0].tag.as_deref(), Some("run1-0:1"));
        d.clear();
        d.spawn(1);
        assert_eq!(
            d.world().loaded[0].tag.as_deref(),
            Some("run1-1:1"),
            "the epoch moved on"
        );
    }

    #[test]
    fn a_tag_of_another_session_is_never_adopted_even_for_a_guest_that_is_shown() {
        let mut d = displays();
        d.spawn(1);
        d.world().unload(); // the plugin's own display is gone from the world (handle dead)
        d.world().foreign(Some("run1-9:1")); // another session's display for the same guest id
        d.sweep();
        assert!(
            d.world()
                .loaded
                .iter()
                .all(|e| e.tag.as_deref() != Some("run1-9:1")),
            "removed, not taken over"
        );
    }
}
