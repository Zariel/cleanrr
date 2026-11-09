use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
};

use crate::{arr::QueueItem, service::candidate_key};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum MediaKey {
    Movie(i64),
    Episode(i64),
}

/// Maps each superseded queue row index to the index of the row kept instead.
///
/// Rows are grouped by movie or episode, and each group keeps its best source.
/// A download that covers several episodes is one source: it stays while it
/// is the best source for any of them, and it is removed only as a whole.
pub fn superseded(items: &[QueueItem]) -> HashMap<usize, usize> {
    let sources: Vec<String> = items.iter().map(candidate_key).collect();

    let mut source_media: HashMap<&str, HashSet<MediaKey>> = HashMap::new();
    let mut groups: BTreeMap<MediaKey, Vec<usize>> = BTreeMap::new();
    for (index, item) in items.iter().enumerate() {
        if !is_healthy_download(item) {
            continue;
        }
        let keys = media_keys(item);
        for &key in &keys {
            groups.entry(key).or_default().push(index);
        }
        source_media
            .entry(sources[index].as_str())
            .or_default()
            .extend(keys);
    }
    let is_pack = |index: usize| source_media[sources[index].as_str()].len() > 1;

    let mut kept_sources = HashSet::new();
    let mut decisions = Vec::new();
    for rows in groups.values() {
        // Prefer quality, then an individual download over a pack, then the
        // oldest row (the queue is sorted by `added`).
        let Some(keep) = rows.iter().copied().max_by(|&a, &b| {
            compare_quality(&items[a], &items[b])
                .then_with(|| is_pack(b).cmp(&is_pack(a)))
                .then_with(|| b.cmp(&a))
        }) else {
            continue;
        };
        kept_sources.insert(sources[keep].as_str());
        decisions.push((rows, keep));
    }

    let mut superseded = HashMap::new();
    for (rows, keep) in decisions {
        for &index in rows {
            if !kept_sources.contains(sources[index].as_str()) {
                superseded.entry(index).or_insert(keep);
            }
        }
    }
    superseded
}

// A pending release has no download yet, and a blocked or failed download
// needs Arr or the user to act. Such rows must neither replace a download
// nor be removed as a duplicate.
fn is_healthy_download(item: &QueueItem) -> bool {
    item.download_id.as_deref().is_some_and(|id| !id.is_empty())
        && item.tracked_download_status.as_deref() == Some("ok")
}

fn media_keys(item: &QueueItem) -> HashSet<MediaKey> {
    let episodes = item.episode_ids.iter().copied().chain(item.episode_id);
    item.movie_id
        .map(MediaKey::Movie)
        .into_iter()
        .chain(episodes.map(MediaKey::Episode))
        .collect()
}

fn compare_quality(a: &QueueItem, b: &QueueItem) -> Ordering {
    // Custom format scores are authoritative when both rows have one.
    let score = match (a.custom_format_score, b.custom_format_score) {
        (Some(a), Some(b)) => a.cmp(&b),
        _ => Ordering::Equal,
    };
    score
        .then_with(|| resolution(a).cmp(&resolution(b)))
        .then_with(|| source_rank(a).cmp(&source_rank(b)))
}

fn resolution(item: &QueueItem) -> i32 {
    let quality = &item.quality.quality;
    quality.resolution.unwrap_or_else(|| {
        let name = quality.name.to_lowercase();
        if name.contains("2160") || name.contains("4k") {
            2160
        } else if name.contains("1080") {
            1080
        } else if name.contains("720") {
            720
        } else if name.contains("480") || name.contains("sd") || name.contains("dvd") {
            480
        } else {
            0
        }
    })
}

fn source_rank(item: &QueueItem) -> i32 {
    let name = item.quality.quality.name.to_lowercase();
    if name.contains("remux") {
        4
    } else if name.contains("bluray") || name.contains("blu-ray") {
        3
    } else if name.contains("web") {
        2
    } else if name.contains("hdtv") {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arr::{Quality, QueueQuality};

    fn row(id: i64, download: &str, resolution: i32) -> QueueItem {
        QueueItem {
            id,
            title: Some(format!("release-{id}")),
            download_id: Some(download.to_owned()),
            added: None,
            status: Some("downloading".to_owned()),
            tracked_download_status: Some("ok".to_owned()),
            tracked_download_state: Some("downloading".to_owned()),
            status_messages: Vec::new(),
            movie_id: None,
            episode_id: None,
            episode_ids: Vec::new(),
            quality: QueueQuality {
                quality: Quality {
                    name: format!("WEBDL-{resolution}p"),
                    resolution: Some(resolution),
                },
            },
            custom_format_score: Some(0),
        }
    }

    /// Sonarr API v3 shape: one row per episode, packs share a download ID.
    fn v3(id: i64, download: &str, episode: i64, resolution: i32) -> QueueItem {
        QueueItem {
            episode_id: Some(episode),
            ..row(id, download, resolution)
        }
    }

    /// Sonarr API v5 shape: one row per download.
    fn v5(id: i64, download: &str, episodes: &[i64], resolution: i32) -> QueueItem {
        QueueItem {
            episode_ids: episodes.to_vec(),
            ..row(id, download, resolution)
        }
    }

    fn removed_ids(items: &[QueueItem]) -> Vec<i64> {
        let mut ids: Vec<_> = superseded(items)
            .keys()
            .map(|&index| items[index].id)
            .collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn empty_queue_and_distinct_episodes_are_untouched() {
        assert!(superseded(&[]).is_empty());
        let items = [v3(1, "a", 101, 1080), v3(2, "b", 102, 1080)];
        assert!(removed_ids(&items).is_empty());
    }

    #[test]
    fn lower_individual_duplicate_is_removed() {
        let items = [v3(1, "a", 101, 720), v3(2, "b", 101, 1080)];
        assert_eq!(superseded(&items), HashMap::from([(0, 1)]));
    }

    #[test]
    fn v3_pack_beats_lower_individual() {
        let items = [
            v3(1, "pack", 101, 1080),
            v3(2, "pack", 102, 1080),
            v3(3, "single", 101, 720),
        ];
        assert_eq!(removed_ids(&items), [3]);
    }

    #[test]
    fn v3_pack_stays_while_needed_for_another_episode() {
        for single in [2160, 1080] {
            let items = [
                v3(1, "pack", 101, 1080),
                v3(2, "pack", 102, 1080),
                v3(3, "single", 101, single),
            ];
            assert!(removed_ids(&items).is_empty(), "{single}");
        }
    }

    #[test]
    fn v3_overlapping_packs() {
        let items = [
            v3(1, "pack-a", 101, 2160),
            v3(2, "pack-a", 102, 2160),
            v3(3, "pack-b", 101, 1080),
            v3(4, "pack-b", 102, 1080),
        ];
        assert_eq!(removed_ids(&items), [3, 4]);

        let mut items = items.to_vec();
        items.push(v3(5, "pack-b", 103, 1080));
        assert!(removed_ids(&items).is_empty());
    }

    #[test]
    fn v3_pack_is_removed_only_when_every_episode_is_superseded() {
        let mut items = vec![
            v3(1, "pack", 101, 720),
            v3(2, "pack", 102, 720),
            v3(3, "single-1", 101, 1080),
            v3(4, "single-2", 102, 1080),
        ];
        assert_eq!(removed_ids(&items), [1, 2]);

        items.push(v3(5, "pack", 103, 720));
        assert!(removed_ids(&items).is_empty());
    }

    #[test]
    fn v5_pack_rows_match_v3_behavior() {
        let items = [
            v5(1, "pack", &[101, 102], 720),
            v5(2, "single-1", &[101], 1080),
            v5(3, "single-2", &[102], 1080),
        ];
        assert_eq!(removed_ids(&items), [1]);

        let items = [
            v5(1, "pack", &[101, 102], 1080),
            v5(2, "single", &[101], 720),
        ];
        assert_eq!(removed_ids(&items), [2]);

        let items = [
            v5(1, "pack", &[101, 102], 720),
            v5(2, "single", &[101], 1080),
        ];
        assert!(removed_ids(&items).is_empty());
    }

    #[test]
    fn custom_format_score_outranks_resolution() {
        let mut low_score = v5(1, "a", &[101], 2160);
        low_score.custom_format_score = Some(0);
        let mut high_score = v5(2, "b", &[101], 1080);
        high_score.custom_format_score = Some(1775);
        assert_eq!(removed_ids(&[low_score, high_score]), [1]);
    }

    #[test]
    fn resolution_decides_when_a_score_is_missing() {
        let mut scored = v5(1, "a", &[101], 1080);
        scored.custom_format_score = Some(100);
        let mut unscored = v5(2, "b", &[101], 2160);
        unscored.custom_format_score = None;
        assert_eq!(removed_ids(&[scored, unscored]), [1]);
    }

    #[test]
    fn source_breaks_resolution_ties() {
        let mut web = v5(1, "a", &[101], 1080);
        web.quality.quality.name = "WEBDL-1080p".to_owned();
        let mut bluray = v5(2, "b", &[101], 1080);
        bluray.quality.quality.name = "Bluray-1080p".to_owned();
        assert_eq!(removed_ids(&[web, bluray]), [1]);
    }

    #[test]
    fn resolution_falls_back_to_the_quality_name() {
        let mut named_2160 = v5(1, "a", &[101], 0);
        named_2160.quality.quality = Quality {
            name: "WEBDL-2160p".to_owned(),
            resolution: None,
        };
        let named_1080 = v5(2, "b", &[101], 1080);
        assert_eq!(removed_ids(&[named_2160, named_1080]), [2]);
    }

    #[test]
    fn full_tie_keeps_the_oldest_row() {
        let items = [v5(1, "a", &[101], 1080), v5(2, "b", &[101], 1080)];
        assert_eq!(superseded(&items), HashMap::from([(1, 0)]));
    }

    #[test]
    fn pending_rows_neither_replace_nor_get_removed() {
        for resolution in [720, 2160] {
            let mut pending = v5(1, "", &[101], resolution);
            pending.download_id = None;
            pending.tracked_download_status = None;
            let items = [pending, v5(2, "a", &[101], 1080)];
            assert!(removed_ids(&items).is_empty(), "{resolution}");
        }
    }

    #[test]
    fn blocked_or_failed_rows_neither_replace_nor_get_removed() {
        for status in ["warning", "error"] {
            for resolution in [720, 2160] {
                let mut unhealthy = v3(1, "pack", 101, resolution);
                unhealthy.tracked_download_status = Some(status.to_owned());
                let mut pack_row = v3(2, "pack", 102, resolution);
                pack_row.tracked_download_status = Some(status.to_owned());
                let items = [unhealthy, pack_row, v3(3, "single", 101, 1080)];
                assert!(removed_ids(&items).is_empty(), "{status} {resolution}");
            }
        }
    }

    #[test]
    fn radarr_rows_group_by_movie() {
        let mut low = row(1, "a", 1080);
        low.movie_id = Some(42);
        let mut high = row(2, "b", 2160);
        high.movie_id = Some(42);
        let mut other = row(3, "c", 720);
        other.movie_id = Some(43);
        assert_eq!(removed_ids(&[low, high, other]), [1]);
    }

    #[test]
    fn rows_without_media_ids_are_ignored() {
        let items = [row(1, "a", 720), row(2, "b", 1080)];
        assert!(removed_ids(&items).is_empty());
    }
}
