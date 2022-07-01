use super::{
    extract_element, get_ids_by_prefix, get_one, get_range, get_site_config, has_unread, incr_id,
    into_response, ivec_to_u64, md2html, set_index, timestamp_to_date, u64_to_ivec,
    u8_slice_to_u64, Claim, IterType, PageData, ParamsPage, Solo, User, ValidatedForm, SEP,
};
use crate::error::AppError;
use askama::Template;
use axum::{
    extract::{Extension, Path, Query, TypedHeader},
    headers::Cookie,
    response::{IntoResponse, Redirect},
};
use bincode::config::standard;
use serde::Deserialize;
use sled::Db;
use time::OffsetDateTime;
use validator::Validate;

#[derive(Deserialize, Validate)]
pub(crate) struct SoloForm {
    #[validate(length(min = 1, max = 1000))]
    content: String,
    visibility: String,
}

#[derive(Template)]
#[template(path = "solo.html", escape = "none")]
struct SoloPage<'a> {
    page_data: PageData<'a>,
    solos: Vec<SoloOut>,
    uid: u64,
    username: String,
    anchor: usize,
    n: usize,
    is_desc: bool,
    is_following: bool,
    filter: Option<String>,
    tag: Option<String>,
}
struct SoloOut {
    uid: u64,
    username: String,
    content: String,
    created_at: String,
    visibility: u64,
}

fn can_visit_solo(visibility: u64, followers: &[u64], solo_uid: u64, current_uid: u64) -> bool {
    visibility == 0
        || (visibility == 10 && followers.contains(&solo_uid))
        || (visibility == 20 && solo_uid == current_uid)
}

#[derive(Deserialize)]
pub(crate) struct SoloParams {
    anchor: Option<usize>,
    is_desc: Option<bool>,
    filter: Option<String>,
    tag: Option<String>,
}

/// `GET /solo/:uid` solo page
pub(crate) async fn solo(
    Extension(db): Extension<Db>,
    cookie: Option<TypedHeader<Cookie>>,
    Path(uid): Path<u64>,
    Query(params): Query<SoloParams>,
) -> Result<impl IntoResponse, AppError> {
    let site_config = get_site_config(&db)?;
    let claim = cookie.and_then(|cookie| Claim::get(&db, &cookie, &site_config));

    let n = site_config.per_page;
    let anchor = params.anchor.unwrap_or(0);
    let is_desc = params.is_desc.unwrap_or(true);
    let page_params = ParamsPage { anchor, n, is_desc };

    let mut is_following = false;
    let mut index = Vec::with_capacity(n);
    let mut followers = Vec::new();
    let mut current_uid = 0;
    if let Some(ref claim) = claim {
        let following_k = [&u64_to_ivec(claim.uid), &SEP, &u64_to_ivec(uid)].concat();
        if db.open_tree("user_following")?.contains_key(&following_k)? {
            is_following = true;
        }

        if let Ok(v) = get_ids_by_prefix(&db, "user_followers", u64_to_ivec(claim.uid), None) {
            followers = v;
        }
        current_uid = claim.uid;
        followers.push(claim.uid);
    }

    match params.filter.as_deref() {
        Some("Following") => {
            if let Some(ref claim) = claim {
                if let Ok(uids) =
                    get_ids_by_prefix(&db, "user_following", u64_to_ivec(claim.uid), None)
                {
                    index = get_solos_by_uids(&db, &uids, &followers, current_uid, &page_params)?;
                };
            }
        }
        _ => {
            if let Some(ref hashtag) = params.tag {
                index = get_ids_by_prefix(&db, "hashtags", hashtag, Some(&page_params))?;
            } else if uid == 0 {
                index = get_all_solos(&db, "solo_timeline", &followers, current_uid, &page_params)?
            } else {
                index = get_solos_by_uids(&db, &[uid], &followers, current_uid, &page_params)?;
            }
        }
    }

    let mut solo_outs = Vec::with_capacity(index.len());
    if !index.is_empty() {
        for sid in index {
            let solo: Solo = get_one(&db, "solos", sid)?;
            let user: User = get_one(&db, "users", solo.uid)?;
            let date = timestamp_to_date(solo.created_at)?;

            let solo_out = SoloOut {
                uid: solo.uid,
                username: user.username,
                content: solo.content,
                created_at: date,
                visibility: solo.visibility,
            };

            solo_outs.push(solo_out);
        }
    }

    let filter = if claim.is_none() { None } else { params.filter };

    let has_unread = if let Some(ref claim) = claim {
        has_unread(&db, claim.uid)?
    } else {
        false
    };

    let username = if uid > 0 {
        let user: User = get_one(&db, "users", uid)?;
        user.username
    } else {
        "All".to_owned()
    };
    let page_data = PageData::new("Solo", &site_config.site_name, claim, has_unread);

    let solo_page = SoloPage {
        page_data,
        solos: solo_outs,
        uid,
        username,
        anchor,
        n,
        is_desc,
        is_following,
        filter,
        tag: params.tag,
    };
    Ok(into_response(&solo_page, "html"))

    // TODO: solo Delete
}

fn get_all_solos(
    db: &Db,
    timeline_tree: &str,
    followers: &[u64],
    current_uid: u64,
    page_params: &ParamsPage,
) -> Result<Vec<u64>, AppError> {
    let tree = db.open_tree(timeline_tree)?;
    let mut count: usize = 0;
    let mut result = Vec::with_capacity(page_params.n);

    let iter = if page_params.is_desc {
        IterType::Rev(tree.iter().rev())
    } else {
        IterType::Iter(tree.iter())
    };
    for i in iter {
        // kv_pair: sid = uid#visibility
        let (k, v) = i?;
        let mut value = v.splitn(2, |num| *num == 35);
        let solo_uid = u8_slice_to_u64(value.next().unwrap());
        let visibility = u8_slice_to_u64(value.next().unwrap());
        if can_visit_solo(visibility, followers, solo_uid, current_uid) {
            if count < page_params.anchor {
                count += 1;
                continue;
            } else {
                result.push(ivec_to_u64(&k));
            }
        }

        if result.len() == page_params.n {
            break;
        }
    }
    Ok(result)
}

fn get_solos_by_uids(
    db: &Db,
    uids: &[u64],
    followers: &[u64],
    current_uid: u64,
    page_params: &ParamsPage,
) -> Result<Vec<u64>, AppError> {
    let mut sids = Vec::with_capacity(page_params.n);
    for uid in uids {
        let prefix = u64_to_ivec(*uid);
        // kv_pair: uid#idx = sid#visibility
        for i in db.open_tree("user_solos_idx")?.scan_prefix(prefix) {
            let (_, v) = i?;
            let mut value = v.splitn(2, |num| *num == 35);
            let sid = u8_slice_to_u64(value.next().unwrap());
            let visibility = u8_slice_to_u64(value.next().unwrap());
            if can_visit_solo(visibility, followers, *uid, current_uid) {
                sids.push(sid);
            }
        }
    }
    let (start, end) = get_range(sids.len(), page_params);
    sids = sids[start - 1..end].to_vec();
    if page_params.is_desc {
        sids.reverse();
    }
    Ok(sids)
}

/// `POST /solo/:uid` solo page
pub(crate) async fn solo_post(
    Extension(db): Extension<Db>,
    ValidatedForm(input): ValidatedForm<SoloForm>,
    cookie: Option<TypedHeader<Cookie>>,
) -> Result<impl IntoResponse, AppError> {
    let site_config = get_site_config(&db)?;
    let claim = cookie
        .and_then(|cookie| Claim::get(&db, &cookie, &site_config))
        .ok_or(AppError::NonLogin)?;

    let visibility = match input.visibility.as_str() {
        "Everyone" => 0,
        "Following" => 10,
        "Just me" => 20,
        _ => unreachable!(),
    };

    let uid = claim.uid;

    let sid = incr_id(&db, "solos_count")?;
    let sid_ivec = u64_to_ivec(sid);
    let mut content = input.content;
    let mut hashtags = Vec::new();

    // TODO: hashtag per user, note-taking
    if visibility == 0 {
        hashtags = extract_element(&content, 5, '#');
        if !hashtags.is_empty() {
            let hashtags_tree = db.open_tree("hashtags")?;
            for hashtag in &hashtags {
                let k = [hashtag.as_bytes(), &SEP, &sid_ivec].concat();
                hashtags_tree.insert(k, &[])?;
            }
        }
        for tag in &hashtags {
            let tag_link = format!("[{}](/solo/user/0?tag={})", tag, tag);
            content = content.replace(tag, &tag_link);
        }
    }

    let content = md2html(&content)?;

    let created_at = OffsetDateTime::now_utc().unix_timestamp();
    let solo = Solo {
        sid,
        uid,
        visibility,
        content,
        hashtags,
        created_at,
    };

    let solo_encode = bincode::encode_to_vec(&solo, standard())?;

    db.open_tree("solos")?.insert(&sid_ivec, solo_encode)?;
    let target = [&sid_ivec, &SEP, &u64_to_ivec(visibility)].concat();
    set_index(&db, "user_solos_count", uid, "user_solos_idx", target)?;

    // kv_pair: sid = uid#visibility
    let v = [&u64_to_ivec(claim.uid), &SEP, &u64_to_ivec(visibility)].concat();
    db.open_tree("solo_timeline")?.insert(&sid_ivec, v)?;

    let target = format!("/solo/user/{}", uid);
    Ok(Redirect::to(&target))
}
