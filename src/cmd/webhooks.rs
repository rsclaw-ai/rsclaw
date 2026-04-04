use anyhow::Result;

use crate::cli::WebhooksCommand;

pub async fn cmd_webhooks(sub: WebhooksCommand) -> Result<()> {
    match sub {
        WebhooksCommand::Gmail => cmd_webhooks_gmail().await,
    }
}

async fn cmd_webhooks_gmail() -> Result<()> {
    println!("Gmail Pub/Sub webhook setup");
    println!();
    println!("prerequisites:");
    println!("  1. A Google Cloud project with Pub/Sub API enabled");
    println!("  2. A Gmail API OAuth2 client (or service account with domain-wide delegation)");
    println!("  3. The rsclaw gateway running and reachable from the internet");
    println!();
    println!("steps:");
    println!("  1. Create a Pub/Sub topic:");
    println!("     gcloud pubsub topics create rsclaw-gmail");
    println!();
    println!("  2. Grant Gmail publish rights:");
    println!("     gcloud pubsub topics add-iam-policy-binding rsclaw-gmail \\");
    println!("       --member='serviceAccount:gmail-api-push@system.gserviceaccount.com' \\");
    println!("       --role='roles/pubsub.publisher'");
    println!();
    println!("  3. Create a push subscription pointing to rsclaw:");
    println!("     gcloud pubsub subscriptions create rsclaw-gmail-push \\");
    println!("       --topic=rsclaw-gmail \\");
    println!("       --push-endpoint='https://<your-domain>/api/v1/webhooks/gmail'");
    println!();
    println!("  4. Watch the Gmail mailbox:");
    println!("     Use the Gmail API users.watch() method with topicName:");
    println!("     projects/<project-id>/topics/rsclaw-gmail");
    println!();
    println!("  5. Add the Gmail channel in rsclaw config:");
    println!("     rsclaw channels add --type gmail --name my-gmail");
    println!();
    println!("for detailed docs: https://docs.openclaw.ai/channels/gmail");
    Ok(())
}
