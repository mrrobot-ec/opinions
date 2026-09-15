# Notification & social UI copy

Source of truth for in-app notification strings and related moderation/error copy.
Web renders from matching constants in `web/lib/copy.ts` — no lorem, no placeholders in product UI.

Placeholders: `{pnl}`, `{question}`, `{score}`, `{handle}`, `{tier}`, `{reason}`.

## Notification templates

### resolution_trade

Default (holder, money-first):

```
Paid out {pnl} on '{question}'
```

Holder who also voted (single row; includes score after resolution):

```
Paid out {pnl} on '{question}' · your crowd call scored {score}
```

### resolution_vote

Vote-only participant (no holdings):

```
'{question}' resolved — your crowd call scored {score}
```

### resolution_void

```
'{question}' was voided — positions redeem at neutral value
```

### comment_reply

```
{handle} replied to you
```

### mention

```
{handle} mentioned you
```

### rep_tier_change

```
You reached tier {tier}
```

### curator / admin

CuratorNeeded (admin handles only):

```
Curator action needed on a flagged market
```

Admin report queue hint (reported comments list context):

```
Comment under review — reports reached the shadow threshold
```

## Social / moderation user copy

### Report confirmation

```
Report submitted. Thanks for helping keep the section usable.
```

### Shadow banner (author-only)

```
Only you can see this while it's under review
```

### Blocked reasons (comment rejected)

| Code | Copy |
|------|------|
| empty | Comment can't be empty. |
| too_long | Comment is too long. |
| rate | You're commenting too fast — try again in a moment. |
| links | Too many links — remove some and try again. |
| duplicate | That looks like a repeat of something you just posted. |

### Error copy

| Situation | Copy |
|-----------|------|
| ThreadTooDeep | This thread is as deep as it goes — reply higher up. |
| duplicate vote | You already voted on this comment. |
| reporter-floor rejection | You can't report yet — account age or tier is too low. |
| report velocity | Too many reports too quickly — slow down. |
| not visible | That comment isn't available. |

## Toast posture

Live toasts fire **only** for:

- `resolution_trade`
- `resolution_vote`
- `resolution_void`
- `rep_tier_change`

`comment_reply` and `mention` update the unread badge only (no toast).
