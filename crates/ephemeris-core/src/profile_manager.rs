use crate::{Profile, ProfileId};
use std::sync::{Arc, Mutex};

/// Active profile management with state persistence.
pub struct ProfileManager {
    profiles: Arc<Mutex<Vec<Profile>>>,
    active_profile_id: Arc<Mutex<Option<ProfileId>>>,
}

impl ProfileManager {
    pub fn new() -> Self {
        ProfileManager {
            profiles: Arc::new(Mutex::new(Vec::new())),
            active_profile_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn create_profile(&self, name: impl Into<String>) -> crate::Result<Profile> {
        let profile = Profile::new(name);
        self.profiles.lock().unwrap().push(profile.clone());
        Ok(profile)
    }

    pub fn list_profiles(&self) -> crate::Result<Vec<Profile>> {
        Ok(self.profiles.lock().unwrap().clone())
    }

    /// Rename a profile in place.  Returns `false` if no profile with that id exists.
    pub fn rename_profile(
        &self,
        id: ProfileId,
        new_name: impl Into<String>,
    ) -> crate::Result<bool> {
        let new_name = new_name.into();
        let mut profiles = self.profiles.lock().unwrap();
        match profiles.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.name = new_name;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Delete a profile.  Returns `false` if no profile with that id exists.
    /// If the deleted profile is active, the active profile is cleared.
    pub fn delete_profile(&self, id: ProfileId) -> crate::Result<bool> {
        let mut profiles = self.profiles.lock().unwrap();
        let before = profiles.len();
        profiles.retain(|p| p.id != id);
        let removed = profiles.len() < before;
        drop(profiles);
        if removed {
            let mut active = self.active_profile_id.lock().unwrap();
            if *active == Some(id) {
                *active = None;
            }
        }
        Ok(removed)
    }

    pub fn set_active_profile(&self, profile_id: ProfileId) -> crate::Result<()> {
        *self.active_profile_id.lock().unwrap() = Some(profile_id);
        Ok(())
    }

    pub fn active_profile(&self) -> crate::Result<Option<ProfileId>> {
        Ok(*self.active_profile_id.lock().unwrap())
    }
}

impl Clone for ProfileManager {
    fn clone(&self) -> Self {
        ProfileManager {
            profiles: Arc::clone(&self.profiles),
            active_profile_id: Arc::clone(&self.active_profile_id),
        }
    }
}

impl Default for ProfileManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_list_profiles() {
        let mgr = ProfileManager::new();
        mgr.create_profile("Work").expect("should create");
        let profiles = mgr.list_profiles().expect("should list");
        assert_eq!(profiles.len(), 1);
    }

    #[test]
    fn set_and_get_active_profile() {
        let mgr = ProfileManager::new();
        let profile = mgr.create_profile("Personal").expect("should create");
        mgr.set_active_profile(profile.id).expect("should set");
        let active = mgr.active_profile().expect("should get");
        assert_eq!(active, Some(profile.id));
    }

    #[test]
    fn rename_profile_found() {
        let mgr = ProfileManager::new();
        let p = mgr.create_profile("Old").unwrap();
        assert!(mgr.rename_profile(p.id, "New").unwrap());
        let profiles = mgr.list_profiles().unwrap();
        assert_eq!(profiles[0].name, "New");
    }

    #[test]
    fn rename_profile_not_found() {
        let mgr = ProfileManager::new();
        assert!(!mgr.rename_profile(ProfileId::new(), "Ghost").unwrap());
    }

    #[test]
    fn delete_profile_removes_it() {
        let mgr = ProfileManager::new();
        let p = mgr.create_profile("Work").unwrap();
        assert!(mgr.delete_profile(p.id).unwrap());
        assert!(mgr.list_profiles().unwrap().is_empty());
        assert!(!mgr.delete_profile(p.id).unwrap(), "second delete is false");
    }

    #[test]
    fn delete_active_profile_clears_active() {
        let mgr = ProfileManager::new();
        let p = mgr.create_profile("Active").unwrap();
        mgr.set_active_profile(p.id).unwrap();
        mgr.delete_profile(p.id).unwrap();
        assert!(mgr.active_profile().unwrap().is_none());
    }

    #[test]
    fn clone_shares_state() {
        let mgr = ProfileManager::new();
        let cloned = mgr.clone();
        mgr.create_profile("Shared").unwrap();
        assert_eq!(cloned.list_profiles().unwrap().len(), 1);
    }
}
