use dotenv::dotenv;
use flowsnet_platform_sdk::logger;
use github_flows::{
    get_octo, listen_to_event,
    octocrab::models::events::payload::{IssueCommentEventAction, IssuesEventAction},
    octocrab::models::CommentId,
    EventPayload, GithubLogin
};
use http_req::{
    request::{Method, Request},
    uri::Uri,
};
use openai_flows::{
    chat::{ChatModel, ChatOptions},
    OpenAIFlows, FlowsAccount,
};
use std::env;

//  The soft character limit of the input context size
//   the max token size or word count for GPT4 is 8192
//   the max token size or word count for GPT35Turbo is 4096
static CHAR_SOFT_LIMIT : usize = 18000;
// static MODEL : ChatModel = ChatModel::GPT35Turbo;
static MODEL : ChatModel = ChatModel::GPT4;

#[no_mangle]
#[tokio::main(flavor = "current_thread")]
pub async fn run() -> anyhow::Result<()> {
    dotenv().ok();
    logger::init();
    log::debug!("Running github-pr-review/playground");

    let owner = env::var("github_owner").unwrap_or("flows-network".to_string());
    let repo = env::var("github_repo").unwrap_or("review-any-pr-with-chatgpt".to_string());
    let trigger_phrase = env::var("trigger_phrase").unwrap_or("flows review".to_string());

    listen_to_event(
        &GithubLogin::Default,
        &owner,
        &repo,
        vec!["issue_comment", "issues"],
        |payload| handler(&owner, &repo, &trigger_phrase, payload),
    )
    .await;

    Ok(())
}

async fn handler(
    owner: &str,
    repo: &str,
    trigger_phrase: &str,
    payload: EventPayload,
) {
    // log::debug!("Received payload: {:?}", payload);
    let (pull_url, issue_number, _contributor) = match payload {
        EventPayload::IssuesEvent(e) => {
            if e.action != IssuesEventAction::Opened {
                // Only responds to newly opened issues
                log::debug!("Received an issue event that is NOT Opened. Ignore");
                return;
            }
            (e.issue.title, e.issue.number, e.issue.user.login)
        },

        EventPayload::IssueCommentEvent(e) => {
            if e.action == IssueCommentEventAction::Deleted {
                log::debug!("Deleted issue comment");
                return;
            }
            log::debug!("Other event for issue comment");

            let body = e.comment.body.unwrap_or_default();

            // TODO: Makeshift but operational
            if body.starts_with("Hello, I am a [code review bot]") {
                log::info!("Ignore comment via bot");
                return;
            };

            if !body.to_lowercase().contains(&trigger_phrase.to_lowercase()) {
                log::info!("Ignore the comment without magic words");
                return;
            }

            (e.issue.title, e.issue.number, e.issue.user.login)
        }
        _ => return,
    };

    let pull_url_components : Vec<&str> = pull_url.split("/").collect();
    if pull_url_components.len() < 5 { return; }
    let pull_number = pull_url_components[pull_url_components.len() - 1].parse::<u64>().unwrap();
    let pull_repo = pull_url_components[pull_url_components.len() - 3];
    let pull_owner = pull_url_components[pull_url_components.len() - 4];
    let chat_id = format!("PR#{pull_number}");
    let system = "You are a senior software developer experienced in code reviews.";
    let mut openai = OpenAIFlows::new();
    openai.set_flows_account(FlowsAccount::Provided("gpt4".to_string()));
    openai.set_retry_times(3);

    let octo = get_octo(&GithubLogin::Default);
    let issues = octo.issues(owner, repo);
    let comment_id: CommentId;
    match issues.create_comment(issue_number, "Hello, I am a [code review bot](https://github.com/flows-network/github-pr-review/) on [flows.network](https://flows.network/).\n\nIt could take a few minutes for me to analyze this PR. Relax, grab a cup of coffee and check back later. Thanks!").await {
        Ok(comment) => {
            comment_id = comment.id;
        }
        Err(error) => {
            log::error!("Error posting comment: {}", error);
            return;
        }
    }

    let pulls = octo.pulls(pull_owner, pull_repo);
    let mut resp = String::new();
    resp.push_str("Hello, I am a [code review bot](https://github.com/flows-network/github-pr-review/) on [flows.network](https://flows.network/). Here are my reviews of changed source code files in this PR.\n\n------\n\n");

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
                    "https://raw.githubusercontent.com/{pull_owner}/{pull_repo}/{}/{}", hash, filename
                );
                let file_uri = Uri::try_from(raw_url.as_str()).unwrap();
                let mut writer = Vec::new();
                match Request::new(&file_uri)
                    .method(Method::GET)
                    .header("Accept", "plain/text")
                    .header("User-Agent", "Flows Network Connector")
                    .send(&mut writer)
                    .map_err(|_e| {}) {
                        Err(_e) => {
                            log::error!("Cannot get file");
                            continue;
                        }
                        _ => {}
                }
                let file_as_text = String::from_utf8_lossy(&writer);
                let t_file_as_text = truncate(&file_as_text, CHAR_SOFT_LIMIT);

                resp.push_str("## [");
                resp.push_str(filename);
                resp.push_str("](");
                resp.push_str(f.blob_url.as_str());
                resp.push_str(")\n\n");

                log::debug!("Sending file to OpenAI: {}", filename);
                let co = ChatOptions {
                    model: MODEL,
                    restart: true,
                    system_prompt: Some(system),
                };
                let question = "Review the following source code and look for potential problems. The code might be truncated. So, do NOT comment on the completeness of the source code.\n\n".to_string() + t_file_as_text;
                match openai.chat_completion(&chat_id, &question, &co).await {
                    Ok(r) => {
                        resp.push_str(&r.choice);
                        resp.push_str("\n\n");
                        log::debug!("Received OpenAI resp for file: {}", filename);
                    }
                    Err(e) => {
                        log::error!("OpenAI returns error for file review for {}: {}", filename, e);
                    }
                }

                log::debug!("Sending patch to OpenAI: {}", filename);
                let co = ChatOptions {
                    model: MODEL,
                    restart: false,
                    system_prompt: Some(system),
                };
                let patch_as_text = f.patch.unwrap_or("".to_string());
                let t_patch_as_text = truncate(&patch_as_text, CHAR_SOFT_LIMIT);
                let question = "The following is a patch. Please summarize key changes.\n\n".to_string() + t_patch_as_text;
                match openai.chat_completion(&chat_id, &question, &co).await {
                    Ok(r) => {
                        resp.push_str(&r.choice);
                        resp.push_str("\n\n");
                        log::debug!("Received OpenAI resp for patch: {}", filename);
                    }
                    Err(e) => {
                        log::error!("OpenAI returns error for patch review for {}: {}", filename, e);
                    }
                }
            }
            resp.push_str("cc ");
            resp.push_str(&pull_url);
            resp.push_str("\n");
        },
        Err(_error) => {
            log::error!("Cannot get file list");
        }
    }

    // Send the entire response to GitHub PR
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
