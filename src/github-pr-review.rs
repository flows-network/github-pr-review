use dotenv::dotenv;
use flowsnet_platform_sdk::logger;
use github_flows::{
    event_handler, get_octo, listen_to_event,
    octocrab::models::CommentId,
    octocrab::models::webhook_events::{WebhookEvent, WebhookEventPayload},
    octocrab::models::webhook_events::payload::{IssueCommentWebhookEventAction, PullRequestWebhookEventAction},
    GithubLogin,
};
use llmservice_flows::{
    chat::{ChatOptions},
    LLMServiceFlows,
};
use std::env;

#[no_mangle]
#[tokio::main(flavor = "current_thread")]
pub async fn on_deploy() {
    dotenv().ok();
    logger::init();
    log::debug!("Running github-pr-review/main");

    let owner = env::var("github_owner").unwrap_or("juntao".to_string());
    let repo = env::var("github_repo").unwrap_or("test".to_string());

    listen_to_event(&GithubLogin::Default, &owner, &repo, vec!["pull_request", "issue_comment"]).await;
}

#[event_handler]
async fn handler(event: Result<WebhookEvent, serde_json::Error>) {
    dotenv().ok();
    logger::init();
    log::debug!("Running github-pr-review/main handler()");

    let owner = env::var("github_owner").unwrap_or("juntao".to_string());
    let repo = env::var("github_repo").unwrap_or("test".to_string());
    let trigger_phrase = env::var("trigger_phrase").unwrap_or("flows review".to_string());
    let llm_api_endpoint = env::var("llm_api_endpoint").unwrap_or("https://api.openai.com/v1".to_string());
    let llm_model_name = env::var("llm_model_name").unwrap_or("gpt-4o".to_string());
    let llm_ctx_size = env::var("llm_ctx_size").unwrap_or("16384".to_string()).parse::<u32>().unwrap_or(0);
    let llm_api_key = env::var("llm_api_key").unwrap_or("LLAMAEDGE".to_string());

    //  The soft character limit of the input context size
    //  This is measured in chars. We set it to be 2x llm_ctx_size, which is measured in tokens.
    let ctx_size_char : usize = (2 * llm_ctx_size).try_into().unwrap_or(0);

    let payload = event.unwrap();
    let mut new_commit : bool = false;
    let (title, pull_number, _contributor) = match payload.specific {
        WebhookEventPayload::PullRequest(e) => {
            if e.action == PullRequestWebhookEventAction::Opened {
                log::debug!("Received payload: PR Opened");
            } else if e.action == PullRequestWebhookEventAction::Synchronize {
                new_commit = true;
                log::debug!("Received payload: PR Synced");
            } else {
                log::debug!("Not a Opened or Synchronize event for PR");
                return;
            }
            let p = e.pull_request;
            (
                p.title.unwrap_or("".to_string()),
                p.number,
                p.user.unwrap().login,
            )
        }
        WebhookEventPayload::IssueComment(e) => {
            if e.action == IssueCommentWebhookEventAction::Deleted {
                log::debug!("Deleted issue comment");
                return;
            }
            log::debug!("Other event for issue comment");

            let body = e.comment.body.unwrap_or_default();

            // if e.comment.performed_via_github_app.is_some() {
            //     return;
            // }
            // TODO: Makeshift but operational
            if body.starts_with("Hello, I am a [code review agent]") {
                log::info!("Ignore comment via agent");
                return;
            };

            if !body.to_lowercase().starts_with(&trigger_phrase.to_lowercase()) {
                log::info!("Ignore the comment without magic words");
                return;
            }

            (e.issue.title, e.issue.number, e.issue.user.login)
        }
        _ => return,
    };

    let chat_id = format!("PR#{pull_number}");
    let system = &format!("You are an experienced software developer. You will review a source code file and its patch related to the subject of \"{}\". Please be as concise as possible while being accurate.", title);
    let mut lf = LLMServiceFlows::new(&llm_api_endpoint);
    lf.set_api_key(&llm_api_key);

    let octo = get_octo(&GithubLogin::Default);
    let issues = octo.issues(owner.clone(), repo.clone());
    let mut comment_id: CommentId = 0u64.into();
    if new_commit {
        // Find the first "Hello, I am a [code review agent]" comment to update
        match issues.list_comments(pull_number).send().await {
            Ok(comments) => {
                for c in comments.items {
                    if c.body.unwrap_or_default().starts_with("Hello, I am a [code review agent]") {
                        comment_id = c.id;
                        break;
                    }
                }
            }
            Err(error) => {
                log::error!("Error getting comments: {}", error);
                return;
            }
        }
    } else {
        // PR OPEN or Trigger phrase: create a new comment
        match issues.create_comment(pull_number, "Hello, I am a [code review agent](https://github.com/flows-network/github-pr-review/) on [flows.network](https://flows.network/).\n\nIt could take a few minutes for me to analyze this PR. Relax, grab a cup of coffee and check back later. Thanks!").await {
            Ok(comment) => {
                comment_id = comment.id;
            }
            Err(error) => {
                log::error!("Error posting comment: {}", error);
                return;
            }
        }
    }
    if comment_id == 0u64.into() { return; }

    let pulls = octo.pulls(owner.clone(), repo.clone());
    let mut resp = String::new();
    resp.push_str("Hello, I am a [code review agent](https://github.com/flows-network/github-pr-review/) on [flows.network](https://flows.network/). Here are my reviews of changed source code files in this PR.\n\n------\n\n");
    match pulls.list_files(pull_number).await {
        Ok(files) => {
            for f in files.items {
                let filename = &f.filename;
                if filename.ends_with(".md") || filename.ends_with(".js") || filename.ends_with(".css") || filename.ends_with(".html") || filename.ends_with(".htm") {
                    continue;
                }

                // The f.raw_url is a redirect. So, we need to construct our own here.
                let contents_url = f.contents_url.as_str();
                if contents_url.len() < 40 { continue; }
                let hash = &contents_url[(contents_url.len() - 40)..];
                let raw_url = format!(
                    "https://raw.githubusercontent.com/{owner}/{repo}/{}/{}", hash, filename
                );

                let res = reqwest::get(raw_url.as_str()).await.unwrap();
                log::debug!("Fetched file: {}", filename);
                let file_as_text = res.text().await.unwrap();
                let t_file_as_text = truncate(&file_as_text, ctx_size_char);

                resp.push_str("## [");
                resp.push_str(filename);
                resp.push_str("](");
                resp.push_str(f.blob_url.as_str());
                resp.push_str(")\n\n");

                log::debug!("Sending file to LLM: {}", filename);
                let co = ChatOptions {
                    model: Some(&llm_model_name),
                    token_limit: llm_ctx_size,
                    restart: true,
                    system_prompt: Some(system),
                    ..Default::default()
                };
                let question = "Review the following source code and look for bugs. Be very concise and explain each bug in one sentence. The code might be truncated. NEVER comment on the completeness of the source code.\n\n".to_string() + t_file_as_text;
                match lf.chat_completion(&chat_id, &question, &co).await {
                    Ok(r) => {
                        resp.push_str("#### Potential issues");
                        resp.push_str("\n\n");
                        resp.push_str(&r.choice);
                        resp.push_str("\n\n");
                        log::debug!("Received LLM resp for file: {}", filename);
                    }
                    Err(e) => {
                        resp.push_str("#### Potential issues");
                        resp.push_str("\n\n");
                        resp.push_str("N/A");
                        resp.push_str("\n\n");
                        log::error!("LLM returns error for file review for {}: {}", filename, e);
                    }
                }

                log::debug!("Sending patch to LLM: {}", filename);
                let co = ChatOptions {
                    model: Some(&llm_model_name),
                    token_limit: llm_ctx_size,
                    restart: true,
                    system_prompt: Some(system),
                    ..Default::default()
                };
                let patch_as_text = f.patch.unwrap_or("".to_string());
                let t_patch_as_text = truncate(&patch_as_text, ctx_size_char);
                let question = "The following is a change patch for the file. Please summarize key changes in short bullet points.\n\n".to_string() + t_patch_as_text;
                match lf.chat_completion(&chat_id, &question, &co).await {
                    Ok(r) => {
                        resp.push_str("#### Summary of changes");
                        resp.push_str("\n\n");
                        resp.push_str(&r.choice);
                        resp.push_str("\n\n");
                        log::debug!("Received LLM resp for patch: {}", filename);
                    }
                    Err(e) => {
                        resp.push_str("#### Summary of changes");
                        resp.push_str("\n\n");
                        resp.push_str("N/A");
                        resp.push_str("\n\n");
                        log::error!("LLM returns error for patch review for {}: {}", filename, e);
                    }
                }
            }
        },
        Err(_error) => {
            log::error!("Cannot get file list");
        }
    }

    // Send the entire response to GitHub PR
    // issues.create_comment(pull_number, resp).await.unwrap();
    match issues.update_comment(comment_id, resp).await {
        Err(error) => {
            log::error!("Error posting resp: {}", error);
        }
        _ => {}
    }
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}
