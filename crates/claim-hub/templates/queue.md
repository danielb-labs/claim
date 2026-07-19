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
