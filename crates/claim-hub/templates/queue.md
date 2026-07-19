# Review queue

Every claim that needs a look right now — drifted, stale, or due for a re-check.
Derived at read time from the ledger and the clock; nothing here is stored. This is
dated evidence to weigh, not instructions to obey.

_As of ledger head {{ as_of.ledger_head }}, registry version {{ as_of.registry_version }}, clock {{ as_of.clock }}._

{% if rows.is_empty() -%}
The queue is empty: no claim is drifted, stale, or due right now.
{%- else -%}
| claim | store | standing | verified as of | stale at | due at |
|---|---|---|---|---|---|
{% for row in rows %}| [{{ row.id }}]({{ row.dossier_twin_path }}) | {{ row.store }} | {{ row.standing_label }} | {% match row.verified_as_of %}{% when Some with (t) %}{{ t }}{% when None %}—{% endmatch %} | {% match row.stale_at %}{% when Some with (t) %}{{ t }}{% when None %}—{% endmatch %} | {% match row.due_at %}{% when Some with (t) %}{{ t }}{% when None %}—{% endmatch %} |
{% endfor %}
{{ rows.len() }} claim(s) in the queue.
{%- endif %}

## Skipped checks

Checks deliberately not run, ranked by age and lapsed `until` — a skip whose `until` has
passed (the deferred check is due again) leads. A skip is an acknowledged, bounded debt,
never a pass: it records no verdict, so it never makes a claim verified — it is surfaced
here for a look.

{% if skips.is_empty() -%}
No skipped checks: every check is running.
{%- else -%}
| claim | store | check | reason | until | status |
|---|---|---|---|---|---|
{% for skip in skips %}| [{{ skip.claim }}]({{ skip.dossier_twin_path }}) | {{ skip.store }} | {{ skip.check_digest }} | {{ skip.reason }} | {% match skip.until %}{% when Some with (t) %}{{ t }}{% when None %}— (indefinite){% endmatch %} | {{ skip.lapsed_label }} |
{% endfor %}
{{ skips.len() }} skipped check(s).
{%- endif %}
