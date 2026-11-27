<script>
  import ChatGptUsage from "./ChatGptUsage.svelte";
  import ChatGptLiveStats from "./ChatGptLiveStats.svelte";

  let { item, index, action } = $props();
  let expanded = $state(false);
  let usage = $state(null);
  const date = (value) => new Date(value * 1000).toLocaleString("ru-RU", {
    day: "numeric", month: "short", hour: "2-digit", minute: "2-digit"
  });
  const windows = [["five_hour", "5 ч"], ["weekly", "Неделя"]];
</script>

<details class="cg-account" bind:open={expanded}>
  <summary>
    <span class="account-heading">
      <b class="account-email">{item.email || `Аккаунт ${index + 1}`}</b>
      <small>{item.plan || "ChatGPT"}{item.active ? " · Активный" : ""}</small>
      {#if item.status === "auth"}<span class="err">Нужен повторный вход</span>
      {:else if item.status === "access"}<span class="err">Сервис недоступен</span>
      {:else if item.status === "limit"}<span class="note">Последний отказ: лимит</span>{/if}
      <span class="reset-times">
        {#if usage?.windows && Object.keys(usage.windows).length > 0}
          {#each windows as [key, label]}
            <span>{label}: {usage.windows[key]?.reset_at ? `сброс ${date(usage.windows[key].reset_at)}` : "срок сброса неизвестен"}</span>
          {/each}
        {:else}
          <span>{usage ? "Сроки сброса недоступны" : "Проверяю сроки сброса…"}</span>
        {/if}
        {#if usage?.error}<span>Не удалось обновить данные</span>{/if}
      </span>
    </span>
    <span class="chevron" aria-hidden="true">›</span>
  </summary>
  <div class="account-details">
      {#each Object.entries(item.services || {}) as [service, restriction]}
        <span class="note">{service === "voice" ? "GPT-Live" : "Codex"} · {restriction.status === "limit" ? "последний отказ по лимиту" : "нет доступа"}
          {#if restriction.retry_after > Date.now() / 1000} · повтор после {date(restriction.retry_after)}{/if}
        </span>
      {/each}
    <ChatGptUsage accountId={item.id} bind:usage />
    {#if expanded}<ChatGptLiveStats stats={item.live_stats} accountId={item.id} />{/if}
    <div class="row">
      {#if !item.active}<button type="button" class="btn" data-cg-select={item.id} onclick={action}>Сделать активным</button>{/if}
      <button type="button" class="btn ghost danger" data-cg-remove={item.id} onclick={action}>Удалить</button>
    </div>
  </div>
</details>

<style>
  .cg-account{border:1px solid var(--line,#5555);border-radius:12px}
  summary{display:flex;align-items:center;gap:12px;padding:12px 14px;cursor:pointer;list-style:none}
  summary::-webkit-details-marker{display:none}
  summary:focus-visible{outline:2px solid var(--accent,#f87a25);outline-offset:2px;border-radius:12px}
  .account-heading{display:grid;gap:4px;min-width:0;flex:1}
  .account-email{overflow-wrap:anywhere}
  small,.reset-times{font-size:12px;line-height:1.5;color:var(--muted,#a5a5b2)}
  .reset-times{display:flex;flex-wrap:wrap;gap:2px 14px}
  .account-heading .err,.account-heading .note{margin:0;font-size:12px}
  .chevron{font-size:24px;line-height:1;transition:transform .15s}
  details[open] .chevron{transform:rotate(90deg)}
  .account-details{padding:0 14px 14px}
  .row{margin-top:10px;flex-wrap:wrap}
</style>
