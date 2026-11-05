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
}

pub struct Displays<W: DisplayWorld> {
    world: W,
    /// Unique per run of the server, so a tag written by an earlier run is never the current session's.
    run: String,
    /// Bumped each time the guest's entities are cleared, so one session's tags are not the next one's.
    epoch: u32,
    shown: HashMap<u32, Shown<W::Handle>>,
}

impl<W: DisplayWorld> Displays<W> {
    pub fn new(world: W, run: String) -> Self {
        Self {
            world,
            run,
            epoch: 0,
            shown: HashMap::new(),
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

    pub fn spawn(&mut self, guest: u32) -> bool {
        self.despawn(guest);
        let tag = Tag {
            session: self.session(),
            guest,
        };
        match self.world.create(&tag) {
            Some((handle, server_id)) => {
                self.shown.insert(
                    guest,
                    Shown {
                        handle,
                        server_id,
                        last: None,
                    },
                );
                true
            }
            None => false,
        }
    }

    pub fn pose(&mut self, guest: u32, pos: [f64; 3], yaw: f32, pitch: f32) {
        if let Some(s) = self.shown.get_mut(&guest) {
            self.world.place(&s.handle, pos, yaw, pitch);
            s.last = Some((pos, yaw, pitch));
        }
    }

    pub fn despawn(&mut self, guest: u32) {
        if let Some(s) = self.shown.remove(&guest) {
            self.world.remove(&s.handle);
        }
    }

    /// The session is over: removes every display, the reloaded copies of them too, and starts a new session.
    pub fn clear(&mut self) {
        for (_, s) in self.shown.drain() {
            self.world.remove(&s.handle);
        }
        self.epoch += 1;
        self.sweep();
    }

    /// Looks at the tagged entities of the world (see the module's rules) and returns how many it removed or adopted.
    pub fn sweep(&mut self) -> usize {
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
                if let Some((pos, yaw, pitch)) = s.last {
                    self.world.place(&s.handle, pos, yaw, pitch);
                }
            }
            changed += 1;
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
            self.loaded.push(E {
                id: self.next_id,
                tag: Some(tag.encode()),
                pos: [0.0; 3],
            });
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
}
