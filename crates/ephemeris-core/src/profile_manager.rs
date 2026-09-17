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
}
