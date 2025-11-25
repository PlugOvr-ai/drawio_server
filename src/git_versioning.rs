use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use git2::{Repository, Signature, Commit, Oid};
use sha2::{Sha256, Digest};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub commit_oid: String,
    pub version: u64,
    pub timestamp: DateTime<Utc>,
    pub username: String,
    pub message: String,
    pub file_path: String,
}

#[derive(Debug, Clone)]
struct FileCommitState {
    last_commit_time: Instant,
    last_content_hash: String,
    last_commit_oid: Option<String>,
}

pub struct GitVersionManager {
    data_dir: PathBuf,
    // Track state per file to decide when to commit
    file_states: Arc<RwLock<HashMap<String, FileCommitState>>>,
    // Serialize Git operations to prevent index lock conflicts
    // Using std::sync::Mutex because we need to hold it across spawn_blocking
    git_mutex: Arc<StdMutex<()>>,
    // Configuration
    min_commit_interval: Duration,
    min_content_change_percent: f64,
}

impl GitVersionManager {
    pub fn new(data_dir: PathBuf) -> Result<Self> {
        let repo_path = &data_dir;
        
        // Initialize repository if it doesn't exist
        if !repo_path.join(".git").exists() {
            info!("Initializing new Git repository in {}", repo_path.display());
            Repository::init(repo_path)
                .context("Failed to initialize Git repository")?;
        }
        
        // Configure repository (open, configure, close)
        {
            let repo = Repository::open(repo_path)
                .context("Failed to open Git repository")?;
            let mut config = repo.config()
                .context("Failed to get Git config")?;
            config.set_str("user.name", "Draw.io Server")
                .context("Failed to set Git user.name")?;
            config.set_str("user.email", "server@drawio.local")
                .context("Failed to set Git user.email")?;
        }
        
        // Set up .gitattributes to treat .drawio files appropriately
        let gitattributes_path = repo_path.join(".gitattributes");
        if !gitattributes_path.exists() {
            std::fs::write(&gitattributes_path, "*.drawio -diff -merge\n")
                .context("Failed to create .gitattributes")?;
        }
        
        info!("Git version manager initialized at {}", repo_path.display());
        
        Ok(Self {
            data_dir,
            file_states: Arc::new(RwLock::new(HashMap::new())),
            git_mutex: Arc::new(StdMutex::new(())),
            min_commit_interval: Duration::from_secs(60), // 1 minute minimum
            min_content_change_percent: 5.0, // 5% content change minimum
        })
    }
    
    /// Calculate SHA-256 hash of content
    fn content_hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }
    
    /// Calculate how much content has changed (rough percentage)
    fn content_change_percent(old_content: &str, new_content: &str) -> f64 {
        if old_content.is_empty() {
            return 100.0; // New file
        }
        
        // Simple heuristic: compare lengths and common substrings
        let old_len = old_content.len();
        let new_len = new_content.len();
        let len_diff = (old_len as i64 - new_len as i64).abs() as usize;
        
        // Calculate longest common subsequence ratio (simplified)
        let min_len = old_len.min(new_len);
        if min_len == 0 {
            return 100.0;
        }
        
        // Rough estimate: if lengths differ significantly, it's a big change
        let length_change = (len_diff as f64 / min_len as f64) * 100.0;
        
        // Also check if content is completely different
        if old_content == new_content {
            return 0.0;
        }
        
        // Use a combination of length change and content similarity
        length_change.max(10.0) // At least 10% if content differs
    }
    
    /// Determine if we should create a commit for this change
    async fn should_commit(
        &self,
        file_path: &str,
        new_content: &str,
        force: bool,
    ) -> Result<bool> {
        if force {
            return Ok(true);
        }
        
        let states = self.file_states.read().await;
        let state = states.get(file_path);
        
        let now = Instant::now();
        let new_hash = Self::content_hash(new_content);
        
        match state {
            Some(s) => {
                // Check time interval
                let time_since_last = now.duration_since(s.last_commit_time);
                if time_since_last < self.min_commit_interval {
                    // Check if content changed significantly
                    if s.last_content_hash == new_hash {
                        // No change at all
                        return Ok(false);
                    }
                    
                    // Get old content to compare
                    if let Some(old_oid) = &s.last_commit_oid {
                        if let Ok(old_content) = self.get_version_content(file_path, old_oid).await {
                            let change_percent = Self::content_change_percent(&old_content, new_content);
                            if change_percent < self.min_content_change_percent {
                                info!("Skipping commit for {}: change too small ({:.1}%)", file_path, change_percent);
                                return Ok(false);
                            }
                        }
                    }
                }
                Ok(true)
            }
            None => {
                // First time seeing this file, always commit
                Ok(true)
            }
        }
    }
    
    /// Create a version commit for a file
    pub async fn create_version(
        &self,
        file_path: &str,
        content: &str,
        username: &str,
        version: u64,
        message: Option<&str>,
        force: bool,
    ) -> Result<Option<VersionInfo>> {
        // Check if we should commit
        if !self.should_commit(file_path, content, force).await? {
            return Ok(None);
        }
        
        let file_path_buf = self.data_dir.join(file_path);
        let file_path_str = file_path.to_string();
        let content_str = content.to_string();
        let username_str = username.to_string();
        let message_str = message.map(|s| s.to_string());
        let repo_path = self.data_dir.clone();
        let git_lock = self.git_mutex.clone();
        
        // Run Git operations in blocking thread with lock held
        let result = tokio::task::spawn_blocking(move || {
            // Acquire lock to serialize Git operations (blocking mutex for blocking thread)
            let _guard = git_lock.lock().unwrap();
            // Ensure file exists and has correct content
            if let Some(parent) = file_path_buf.parent() {
                std::fs::create_dir_all(parent)
                    .context("Failed to create parent directory")?;
            }
            std::fs::write(&file_path_buf, &content_str)
                .context("Failed to write file")?;
            
            // Open repository
            let repo = Repository::open(&repo_path)
                .context("Failed to open repository")?;
            
            // Stage the file
            let mut index = repo.index()
                .context("Failed to get repository index")?;
            
            index.add_path(Path::new(&file_path_str))
                .context("Failed to add file to index")?;
            index.write()
                .context("Failed to write index")?;
            
            // Check if there are any changes to commit
            let tree_id = index.write_tree()
                .context("Failed to write tree")?;
            let tree = repo.find_tree(tree_id)
                .context("Failed to find tree")?;
            
            // Get HEAD commit (if exists)
            let parent_commits: Vec<Commit> = if let Ok(head) = repo.head() {
                if let Ok(commit) = head.peel_to_commit() {
                    vec![commit]
                } else {
                    vec![]
                }
            } else {
                vec![]
            };
            
            // Create commit
            let signature = Signature::now(
                &username_str,
                &format!("{}@drawio.local", username_str),
            ).context("Failed to create signature")?;
            
            let commit_message = format!(
                "Version {} by {}\n\n{}\n\nMetadata:\n- File: {}\n- Version: {}\n- User: {}\n- Timestamp: {}",
                version,
                username_str,
                message_str.as_deref().unwrap_or(""),
                file_path_str,
                version,
                username_str,
                Utc::now().to_rfc3339()
            );
            
            let parent_refs: Vec<&Commit> = parent_commits.iter().collect();
            let commit_oid = repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                &commit_message,
                &tree,
                &parent_refs,
            ).context("Failed to create commit")?;
            
            Ok::<String, anyhow::Error>(commit_oid.to_string()) as Result<String, anyhow::Error>
            // Lock is released when _guard is dropped at end of closure
        }).await.context("Failed to spawn blocking task")??;
        
        let commit_oid_str = result;
        info!("Created Git commit {} for file {} (version {})", commit_oid_str, file_path, version);
        
        // Update file state
        {
            let mut states = self.file_states.write().await;
            states.insert(file_path.to_string(), FileCommitState {
                last_commit_time: Instant::now(),
                last_content_hash: Self::content_hash(content),
                last_commit_oid: Some(commit_oid_str.clone()),
            });
        }
        
        Ok(Some(VersionInfo {
            commit_oid: commit_oid_str,
            version,
            timestamp: Utc::now(),
            username: username.to_string(),
            message: message.unwrap_or("").to_string(),
            file_path: file_path.to_string(),
        }))
    }
    
    /// List versions for a file
    pub async fn list_versions(&self, file_path: &str) -> Result<Vec<VersionInfo>> {
        let repo_path = self.data_dir.clone();
        let file_path_str = file_path.to_string();
        
        tokio::task::spawn_blocking(move || {
            let repo = Repository::open(&repo_path)
                .context("Failed to open repository")?;
            let mut revwalk = repo.revwalk()
                .context("Failed to create revwalk")?;
            
            // Start from HEAD
            if let Ok(head) = repo.head() {
                revwalk.push(head.target().unwrap())
                    .context("Failed to push HEAD")?;
            } else {
                return Ok::<Vec<VersionInfo>, anyhow::Error>(vec![]); // No commits yet
            }
            
            revwalk.set_sorting(git2::Sort::TIME)
                .context("Failed to set sort order")?;
            
            let mut versions = Vec::new();
            for oid_result in revwalk {
                let oid = oid_result.context("Failed to get commit OID")?;
                let commit = repo.find_commit(oid)
                    .context("Failed to find commit")?;
                
                // Check if this commit modified our file
                let touches = if commit.parent_count() == 0 {
                    let tree = commit.tree()?;
                    tree.get_path(Path::new(&file_path_str)).is_ok()
                } else {
                    let parent = commit.parent(0)?;
                    let parent_tree = parent.tree()?;
                    let tree = commit.tree()?;
                    let parent_entry = parent_tree.get_path(Path::new(&file_path_str));
                    let current_entry = tree.get_path(Path::new(&file_path_str));
                    match (parent_entry, current_entry) {
                        (Ok(pe), Ok(ce)) => pe.id() != ce.id(),
                        (Err(_), Ok(_)) => true,
                        (Ok(_), Err(_)) => true,
                        (Err(_), Err(_)) => false,
                    }
                };
                
                if touches {
                    let message = commit.message().unwrap_or("");
                    let lines: Vec<&str> = message.lines().collect();
                    let version = lines.get(0)
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                    let username = lines.get(0)
                        .and_then(|l| l.split(" by ").nth(1))
                        .unwrap_or("unknown")
                        .to_string();
                    let msg = lines.get(2).unwrap_or(&"").to_string();
                    let timestamp = DateTime::from_timestamp(
                        commit.time().seconds(),
                        0,
                    ).unwrap_or_else(|| Utc::now());
                    
                    versions.push(VersionInfo {
                        commit_oid: commit.id().to_string(),
                        version,
                        timestamp,
                        username,
                        message: msg,
                        file_path: file_path_str.clone(),
                    });
                }
            }
            
            Ok(versions)
        }).await.context("Failed to spawn blocking task")?
    }
    
    /// Get content of a file at a specific commit
    pub async fn get_version_content(&self, file_path: &str, commit_oid: &str) -> Result<String> {
        let repo_path = self.data_dir.clone();
        let file_path_str = file_path.to_string();
        let commit_oid_str = commit_oid.to_string();
        
        tokio::task::spawn_blocking(move || {
            let oid = Oid::from_str(&commit_oid_str)
                .context("Invalid commit OID")?;
            let repo = Repository::open(&repo_path)
                .context("Failed to open repository")?;
            let commit = repo.find_commit(oid)
                .context("Commit not found")?;
            let tree = commit.tree()
                .context("Failed to get commit tree")?;
            
            let entry = tree.get_path(Path::new(&file_path_str))
                .context("File not found in commit")?;
            let blob = repo.find_blob(entry.id())
                .context("Failed to find blob")?;
            
            String::from_utf8(blob.content().to_vec())
                .context("Invalid UTF-8 in file content")
        }).await.context("Failed to spawn blocking task")?
    }
    
    /// Restore a version (creates new commit with old content)
    pub async fn restore_version(
        &self,
        file_path: &str,
        commit_oid: &str,
        username: &str,
        version: u64,
    ) -> Result<VersionInfo> {
        // Get content from old commit
        let content = self.get_version_content(file_path, commit_oid).await?;
        
        // Create new commit with restored content
        self.create_version(file_path, &content, username, version, 
            Some(&format!("Restored from commit {}", commit_oid)), true).await?
            .context("Failed to create restore commit")
    }
    
    
    /// Push to remote repository
    pub async fn push_to_remote(&self, remote_name: &str, branch: &str) -> Result<()> {
        info!("Pushing to remote '{}' branch '{}'", remote_name, branch);
        
        let repo_path = self.data_dir.clone();
        let remote_name_str = remote_name.to_string();
        let branch_str = branch.to_string();
        let git_lock = self.git_mutex.clone();
        
        tokio::task::spawn_blocking(move || {
            // Acquire lock to serialize Git operations
            let _guard = git_lock.lock().unwrap();
            let repo = Repository::open(&repo_path)
                .context("Failed to open repository")?;
            let mut remote = repo.find_remote(&remote_name_str)
                .context(format!("Remote '{}' not found. Configure it with: git remote add {} <url>", remote_name_str, remote_name_str))?;
            
            // Get current branch
            let refspec = format!("refs/heads/{}:refs/heads/{}", branch_str, branch_str);
            
            remote.push(&[&refspec], None)
                .context("Failed to push to remote")?;
            
            info!("Successfully pushed to remote '{}'", remote_name_str);
            Ok::<(), anyhow::Error>(())
        }).await.context("Failed to spawn blocking task")??;
        
        Ok(())
    }
    
    /// Add or update remote
    pub async fn set_remote(&self, remote_name: &str, url: &str) -> Result<()> {
        let repo_path = self.data_dir.clone();
        let remote_name_str = remote_name.to_string();
        let url_str = url.to_string();
        let git_lock = self.git_mutex.clone();
        
        tokio::task::spawn_blocking(move || {
            // Acquire lock to serialize Git operations
            let _guard = git_lock.lock().unwrap();
            let repo = Repository::open(&repo_path)
                .context("Failed to open repository")?;
            
            // Remove existing remote if it exists
            if repo.find_remote(&remote_name_str).is_ok() {
                repo.remote_delete(&remote_name_str)
                    .context("Failed to delete existing remote")?;
            }
            
            // Create new remote
            repo.remote(&remote_name_str, &url_str)
                .context("Failed to create remote")?;
            
            info!("Set remote '{}' to '{}'", remote_name_str, url_str);
            Ok::<(), anyhow::Error>(())
        }).await.context("Failed to spawn blocking task")??;
        
        Ok(())
    }
    
    
    /// Run garbage collection to optimize repository
    pub async fn gc(&self) -> Result<()> {
        info!("Running Git garbage collection");
        // Note: git2 doesn't have direct GC, but we can use git binary if needed
        // For now, we'll skip this or implement via process
        Ok(())
    }
}


