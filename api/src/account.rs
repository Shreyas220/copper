//! People: browser sign-in, invites, users and API tokens.

use serde::{Deserialize, Serialize};

use crate::user::Role;

/// The browser session cookie kiln sets on sign-in.
pub const SESSION_COOKIE: &str = "copper_session";
/// Cookie-authenticated requests that change anything must carry this
/// header. Browsers won't add a custom header cross-site without a CORS
/// preflight, which kiln never grants, so this blocks request forgery.
pub const CSRF_HEADER: &str = "x-copper";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignIn {
    pub name: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Me {
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub name: String,
    pub role: Role,
    pub projects: Vec<String>,
    /// Unix seconds of the most recent token use or browser session.
    #[serde(default)]
    pub last_seen: Option<i64>,
    /// Can sign in to the console.
    #[serde(default)]
    pub has_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRole {
    pub role: Role,
}

/// An invite either brings in a new person or, naming an existing user,
/// lets them set a new password.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateInvite {
    /// Fix the user name. Required to reset an existing user's password.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<Role>,
    /// Create this project for the new user.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Invite {
    /// One-time code; the link is `/#/invite/<code>`. Only returned once.
    pub code: String,
    pub expires_at: i64,
    pub name: Option<String>,
    pub role: Role,
    pub project: Option<String>,
    /// Whether the name is an existing user resetting their password.
    pub reset: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptInvite {
    pub name: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub created_at: i64,
    #[serde(default)]
    pub last_used: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateToken {
    pub name: String,
}

/// A freshly made token. The secret is shown this once and never again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewToken {
    pub token: Token,
    pub secret: String,
}

/// An SSH public key saved by its owner. A create that names no keys puts the
/// caller's saved keys into the VM. Kiln never holds private keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshKey {
    pub id: String,
    pub name: String,
    /// One line, as in `~/.ssh/id_ed25519.pub`.
    pub public_key: String,
    /// `SHA256:…`, as `ssh-keygen -l` prints it.
    pub fingerprint: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddSshKey {
    pub public_key: String,
    /// Defaults to the key's comment, such as `you@laptop`.
    #[serde(default)]
    pub name: Option<String>,
}

/// Passwords are long enough to resist guessing over the network, and short
/// enough to type.
pub const MIN_PASSWORD_LEN: usize = 10;
