use clap::{Args, Subcommand};

#[derive(Subcommand, Debug)]
pub enum MessageCommand {
    /// Send a message
    Send(MessageSendArgs),
    /// Read recent messages
    Read(MessageReadArgs),
    /// Broadcast a message to multiple targets
    Broadcast(MessageBroadcastArgs),
    /// Edit a message
    Edit(MessageEditArgs),
    /// Delete a message
    Delete(MessageDeleteArgs),
    /// Pin a message
    Pin(MessageTargetArgs),
    /// Unpin a message
    Unpin(MessageTargetArgs),
    /// List pinned messages
    Pins(MessageChannelArgs),
    /// Add or remove a reaction
    React(MessageReactArgs),
    /// List reactions on a message
    Reactions(MessageTargetArgs),
    /// Send a poll
    Poll(MessagePollArgs),
    /// Search messages
    Search(MessageSearchArgs),
    /// Thread actions
    #[command(subcommand)]
    Thread(ThreadCommand),
    /// Voice actions
    #[command(subcommand)]
    Voice(VoiceCommand),
    /// Sticker actions
    #[command(subcommand)]
    Sticker(StickerCommand),
    /// Emoji actions
    #[command(subcommand)]
    Emoji(EmojiCommand),
    /// Ban a member
    Ban(MemberActionArgs),
    /// Kick a member
    Kick(MemberActionArgs),
    /// Timeout a member
    Timeout(TimeoutArgs),
    /// Member actions
    #[command(subcommand)]
    Member(MemberCommand),
    /// Role actions
    #[command(subcommand)]
    Role(RoleCommand),
    /// Fetch channel permissions
    Permissions(MessageChannelArgs),
    /// Channel actions
    #[command(subcommand)]
    Channel(ChannelActionCommand),
    /// Event actions
    #[command(subcommand)]
    Event(EventCommand),
}

#[derive(Args, Debug)]
pub struct MessageSendArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(short = 'm', long)]
    pub message: String,
    #[arg(long)]
    pub media: Option<String>,
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub reply_to: Option<String>,
}

#[derive(Args, Debug)]
pub struct MessageReadArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct MessageBroadcastArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub targets: Vec<String>,
    #[arg(short = 'm', long)]
    pub message: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct MessageEditArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub message_id: String,
    #[arg(short = 'm', long)]
    pub message: String,
}

#[derive(Args, Debug)]
pub struct MessageDeleteArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub message_id: String,
}

#[derive(Args, Debug)]
pub struct MessageTargetArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub message_id: String,
}

#[derive(Args, Debug)]
pub struct MessageChannelArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
}

#[derive(Args, Debug)]
pub struct MessageReactArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub message_id: String,
    #[arg(long)]
    pub emoji: String,
    #[arg(long)]
    pub remove: bool,
}

#[derive(Args, Debug)]
pub struct MessagePollArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub poll_question: String,
    #[arg(long)]
    pub poll_option: Vec<String>,
}

#[derive(Args, Debug)]
pub struct MessageSearchArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub query: String,
    #[arg(long)]
    pub limit: Option<u32>,
}

#[derive(Args, Debug)]
pub struct MemberActionArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub user_id: String,
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Args, Debug)]
pub struct TimeoutArgs {
    #[arg(long)]
    pub channel: Option<String>,
    #[arg(long)]
    pub target: String,
    #[arg(long)]
    pub user_id: String,
    #[arg(long)]
    pub duration: String,
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ThreadCommand {
    /// Create a new thread from a message
    Create {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        message_id: String,
        #[arg(short = 'm', long)]
        message: String,
    },
    /// Reply to an existing thread
    Reply {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        thread_id: String,
        #[arg(short = 'm', long)]
        message: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum VoiceCommand {
    /// Join a voice channel
    Join {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
    },
    /// Leave a voice channel
    Leave {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum StickerCommand {
    /// Send a sticker
    Send {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        sticker_id: String,
    },
    /// List available stickers
    List {
        #[arg(long)]
        channel: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum EmojiCommand {
    /// List available emoji
    List {
        #[arg(long)]
        channel: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum MemberCommand {
    /// List members
    List {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
    },
    /// Get member info
    Info {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        user_id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum RoleCommand {
    /// List roles
    List {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
    },
    /// Assign a role to a member
    Assign {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        role_id: String,
    },
    /// Remove a role from a member
    Remove {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        user_id: String,
        #[arg(long)]
        role_id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ChannelActionCommand {
    /// Create a channel
    Create {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        name: String,
    },
    /// Delete a channel
    Delete {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
    },
    /// Get channel info
    Info {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum EventCommand {
    /// Create a scheduled event
    Create {
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        target: String,
        #[arg(long)]
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long)]
        start: String,
    },
}
