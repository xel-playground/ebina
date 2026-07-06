<script setup>
import { ref, onMounted, onUnmounted } from 'vue'
import { api } from '../api'

// Only Discord exists today — structured as a list so more apps (Telegram,
// etc.) can slot in later without reshaping this panel.
const apps = ref({ discord: null })
const revealed = ref({ discord: false })
let interval = null

async function refreshDiscord() { apps.value.discord = (await api('/discord/pairing')).body }
async function refreshAll() { await refreshDiscord() }

onMounted(() => {
  refreshAll()
  // the code rotates every 60s — poll often enough that what's on screen
  // is never more than a few seconds stale
  interval = setInterval(refreshAll, 5000)
})
onUnmounted(() => clearInterval(interval))
defineExpose({ refresh: refreshAll })
</script>

<template>
  <div>
    <h2>Apps <button class="secondary" @click="refreshAll">Refresh</button></h2>
    <p class="hint">Third-party chat surfaces this agent is paired with — each has an "owner" (who
      `chat_send`'s per-app `target` DMs by default) established via a one-time pairing code.</p>

    <div class="card">
      <h3 style="margin-top:0">👾 Discord</h3>
      <template v-if="apps.discord">
        <template v-if="apps.discord.paired">
          <p>Paired — owner Discord user id <code>{{ apps.discord.user_id }}</code>.</p>
          <p class="hint">Delete `agent-home/logs/discord_owner.json` and restart the gateway to
            un-pair and get a fresh code.</p>
        </template>
        <template v-else>
          <p>Not paired yet. DM the bot this code (message content = just the number):</p>
          <div class="row" style="align-items:center; gap:0.6rem">
            <span style="font-size:2rem; font-weight:bold; letter-spacing:0.1em; font-family:ui-monospace,monospace">
              {{ revealed.discord ? apps.discord.code : '••••••' }}
            </span>
            <button class="secondary" @click="revealed.discord = !revealed.discord">
              {{ revealed.discord ? '🙈 hide' : '👁 show' }}
            </button>
          </div>
          <p class="hint">Rotates every 60s — no rush, just DM whatever's showing when you're ready.
            No Discord bot connected? Check `discord_bot_token` is set in the vault and the gateway
            log for `[discord] connected as ...` — see README's Discord adapter section.</p>
        </template>
      </template>
    </div>
  </div>
</template>
