use dotenv::dotenv;
use flowsnet_platform_sdk::write_error_log;
use github_flows::{
    get_octo, listen_to_event,
    octocrab::models::events::payload::{IssueCommentEventAction, PullRequestEventAction},
    EventPayload,
};
use http_req::{
    request::{Method, Request},
    uri::Uri,
};
use openai_flows::{chat_completion_default_key, ChatModel, ChatOptions};
use std::env;

//  The soft character limit of the input context size
//   the max token size or word count for GPT4 is 8192
//   the max token size or word count for GPT35Turbo is 4096
static CHAR_SOFT_LIMIT : usize = 6000;
static MODEL : ChatModel = ChatModel::GPT35Turbo;
// static MODEL : ChatModel = ChatModel::GPT4;

#[no_mangle]
#[tokio::main(flavor = "current_thread")]
pub async fn run() -> anyhow::Result<()> {
    dotenv().ok();

    let login = env::var("login").unwrap_or("juntao".to_string());
    let owner = env::var("owner").unwrap_or("juntao".to_string());
    let repo = env::var("repo").unwrap_or("test".to_string());
    let trigger_phrase = env::var("trigger_phrase").unwrap_or("flows review".to_string());

    let events = vec!["pull_request", "issue_comment"];
    listen_to_event(&login, &owner, &repo, events, |payload| {
        handler(
            &login,
            &owner,
            &repo,
            &trigger_phrase,
            payload,
        )
    })
    .await;

    Ok(())
}

async fn handler(
    login: &str,
    owner: &str,
    repo: &str,
    trigger_phrase: &str,
    payload: EventPayload,
) {
    let (title, pull_number, _contributor) = match payload {
        EventPayload::PullRequestEvent(e) => {
            if e.action != PullRequestEventAction::Opened {
                write_error_log!("Not a Opened pull event");
                return;
            }
            let p = e.pull_request;
            (
                p.title.unwrap_or("".to_string()),
                p.number,
                p.user.unwrap().login,
            )
        }
        EventPayload::IssueCommentEvent(e) => {
            if e.action == IssueCommentEventAction::Deleted {
                write_error_log!("Deleted issue event");
                return;
            }

            let body = e.comment.body.unwrap_or_default();

            // if e.comment.performed_via_github_app.is_some() {
            //     return;
            // }
            // TODO: Makeshift but operational
            if body.starts_with("Hello, I am a [serverless review bot]") {
                write_error_log!("Ignore comment via bot");
                return;
            };

            if !body.to_lowercase().contains(&trigger_phrase.to_lowercase()) {
                write_error_log!(format!("Ignore the comment, raw: {}", body));
                return;
            }

            (e.issue.title, e.issue.number, e.issue.user.login)
        }
        _ => return,
    };

    let chat_id = format!("PR#{pull_number}");
    let system = &format!("You are an experienced software developer. You will act as a reviewer for a set of files and their patches related to the subject of \"{}\".", title);

    let octo = get_octo(Some(String::from(login)));
    let pulls = octo.pulls(owner, repo);
    let mut resp = String::new();
    resp.push_str("Hello, I am a [serverless review bot](https://github.com/flows-network/github-pr-review/) on [flows.network](https://flows.network/). Here are my reviews of changed files in this PR.\n\n------\n\n");
    match pulls.list_files(pull_number).await {
        Ok(files) => {
            for f in files.items {
                let filename = &f.filename;
                if filename.ends_with(".md") || filename.ends_with(".txt") || filename.ends_with(".html") || filename.ends_with(".htm") {
                    continue;
                }
                resp.push_str("## [");
                resp.push_str(filename);
                resp.push_str("](");
                resp.push_str(f.blob_url.as_str());
                resp.push_str(")\n\n");
                
                // The f.raw_url is a redirect. So, we need to construct our own here.
                let contents_url = f.contents_url.as_str();
                let hash = &contents_url[(contents_url.len() - 40)..];
                let raw_url = format!(
                    "https://raw.githubusercontent.com/{owner}/{repo}/{}/{}", hash, &f.filename
                );
                let file_uri = Uri::try_from(raw_url.as_str()).unwrap();
                let mut writer = Vec::new();
                let _ = Request::new(&file_uri)
                    .method(Method::GET)
                    .header("Accept", "plain/text")
                    .header("User-Agent", "Flows Network Connector")
                    .send(&mut writer)
                    .map_err(|_e| {})
                    .unwrap();
                let file_as_text = String::from_utf8_lossy(&writer);
                let t_file_as_text = truncate(&file_as_text, CHAR_SOFT_LIMIT);

                let co = ChatOptions {
                    model: MODEL,
                    restart: true,
                    system_prompt: Some(system),
                    retry_times: 3,
                };
                let question = "Review the following source code snippet and look for potential problems.\n\n".to_string() + t_file_as_text;
                // resp.push_str(&question);
                // resp.push_str("\n\n");
                if let Some(r) = chat_completion_default_key(&chat_id, &question, &co) {
                    resp.push_str(&r.choice);
                    resp.push_str("\n\n");
                }

                let co = ChatOptions {
                    model: MODEL,
                    restart: false,
                    system_prompt: None,
                    retry_times: 3,
                };
                let patch_as_text = f.patch.unwrap();
                let t_patch_as_text = truncate(&patch_as_text, CHAR_SOFT_LIMIT);
                let question = "The following is a patch. Please summarize key changes.\n\n".to_string() + t_patch_as_text;
                if let Some(r) = chat_completion_default_key(&chat_id, &question, &co) {
                    resp.push_str(&r.choice);
                    resp.push_str("\n\n");
                }
            }
        },
        Err(_error) => {
            write_error_log!("Cannot get file list");
        }
    }

    // Send the entire response to GitHub PR
    let issues = octo.issues(owner, repo);
    issues.create_comment(pull_number, resp).await.unwrap();
}

fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => &s[..idx],
    }
}
