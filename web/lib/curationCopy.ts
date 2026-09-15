/**
 * Curator / content-engine UI strings — mirrors docs/copy/curation.md.
 */

export const CURATION = {
  page_title: "Curation",
  dev_admin_banner: "DEV · admin token required · not production auth",
  need_admin_token:
    "Set an admin token (Dev login → Admin token field) to load the queue.",
  unavailable:
    "Curation APIs aren't available yet — core routes may still be landing.",
  draft_queue: "Draft queue",
  schedule_rail: "Schedule",
  slot_fill: "Slot fill",
  slot_unfilled: "Unfilled slots",
  video_jobs: "Video / poster jobs",
  create_draft: "New draft",
  topic_label: "Topic seed",
  tier_daily: "Daily flagship",
  tier_flash: "Flash (poster-first)",
  source_template: "Source: template",
  source_llm: "Source: LLM",
  fallback_banner:
    "Generated with template fallback — LLM engine was unavailable.",
  approve: "Approve & slot",
  reject: "Reject",
  publish_now: "Publish now",
  publish_now_hint:
    "Only for approved drafts — skips the wait, same publication saga.",
  edit_question: "Question",
  edit_description: "Description",
  edit_script: "Video script",
  no_pending: "No pending drafts.",
  no_schedule: "No reserved slots ahead.",
  no_unfilled: "No recent unfilled slots.",
  conflict_badge: "Slot conflict",
  reserved_badge: "Reserved",
  fill_rate_label: "Fill rate (recent windows)",
  status_pending: "pending",
  status_approved: "approved",
  status_rejected: "rejected",
  status_published: "published",
  status_expired: "expired",
  job_queued: "queued",
  job_rendering: "rendering",
  job_ready: "ready",
  job_attached: "attached",
  job_failed: "failed",
  empty_coming_soon: "No upcoming markets",
  empty_coming_soon_hint:
    "Curators feed the queue — unfilled slots lapse.",
  rail_scheduled: "Scheduled",
  rail_opens_in: "opens",
  poster_first_chip: "Poster-first",
  media_placeholder: "Market card",
  media_loading: "Loading media…",
  share_card: "Share card",
  share_card_hint: "Post-resolution only · real PnL · embeds as image",
  share_card_unavailable: "Share card not available yet.",
  share_card_pre_resolve: "Available after this market resolves.",
  err_no_slot: "No free slot in the horizon — try later or free a reservation.",
  err_pending_publish: "Publish now requires an approved draft.",
  err_generic: "Action failed.",
  flash_poster_note:
    "Flash markets ship poster-first this phase — not full video reels.",
} as const;

export function sourceLabel(source: string): string {
  if (source === "llm") return CURATION.source_llm;
  return CURATION.source_template;
}

export function draftStatusLabel(status: string): string {
  switch (status) {
    case "pending":
      return CURATION.status_pending;
    case "approved":
      return CURATION.status_approved;
    case "rejected":
      return CURATION.status_rejected;
    case "published":
      return CURATION.status_published;
    case "expired":
      return CURATION.status_expired;
    default:
      return status;
  }
}

export function jobStatusLabel(status: string): string {
  switch (status) {
    case "queued":
      return CURATION.job_queued;
    case "rendering":
      return CURATION.job_rendering;
    case "ready":
      return CURATION.job_ready;
    case "attached":
      return CURATION.job_attached;
    case "failed":
      return CURATION.job_failed;
    default:
      return status;
  }
}
