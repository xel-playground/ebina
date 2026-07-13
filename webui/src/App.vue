<script setup>
import { ref } from 'vue'
import { api, token, clearToken } from './api'
import Login from './components/Login.vue'
import Sidebar from './components/Sidebar.vue'
import ChatPanel from './components/ChatPanel.vue'
import StatusPanel from './components/StatusPanel.vue'
import SchedulerPanel from './components/SchedulerPanel.vue'
import ScheduleHistoryPanel from './components/ScheduleHistoryPanel.vue'
import NotesPanel from './components/NotesPanel.vue'
import SoulPanel from './components/SoulPanel.vue'
import CorePanel from './components/CorePanel.vue'
import SkillsPanel from './components/SkillsPanel.vue'
import GrantsPanel from './components/GrantsPanel.vue'
import ReportsPanel from './components/ReportsPanel.vue'
import ConfigPanel from './components/ConfigPanel.vue'
import SecretsPanel from './components/SecretsPanel.vue'
import FileBrowserPanel from './components/FileBrowserPanel.vue'
import EgressPanel from './components/EgressPanel.vue'
import LlmLogsPanel from './components/LlmLogsPanel.vue'
import AppsPanel from './components/AppsPanel.vue'

const loggedIn = ref(false)
const section = ref('chat')

function go(s) { section.value = s }
function logout() {
  clearToken()
  loggedIn.value = false
}

// try the token already in localStorage before showing the login form
if (token()) {
  api('/status').then(({ status }) => { if (status === 200) loggedIn.value = true })
}
</script>

<template>
  <Login v-if="!loggedIn" @logged-in="loggedIn = true" />
  <div v-else class="shell">
    <Sidebar :section="section" @go="go" @logout="logout" />
    <div class="main">
      <!-- v-show, not v-if: switching sidebar tabs must not unmount this —
           unmounting would wipe `sending`/`thinkingText`/`lastTrace` mid-run
           (the in-flight `/api/message` promise keeps running in the
           background regardless, but a fresh mount on navigating back has
           no idea a reply is still pending) -->
      <ChatPanel v-show="section === 'chat'" />
      <StatusPanel v-if="section === 'status'" />
      <SchedulerPanel v-if="section === 'scheduler'" />
      <ScheduleHistoryPanel v-if="section === 'schedule-history'" />
      <NotesPanel v-if="section === 'notes'" />
      <SoulPanel v-if="section === 'soul'" />
      <CorePanel v-if="section === 'core'" />
      <SkillsPanel v-if="section === 'skills'" />
      <GrantsPanel v-if="section === 'grants'" />
      <ReportsPanel v-if="section === 'report'" />
      <ConfigPanel v-if="section === 'config'" />
      <SecretsPanel v-if="section === 'secrets'" />
      <FileBrowserPanel v-if="section === 'logs'" key="logs" root="logs" api-base="/logs" title="Logs"
        description="Raw browser over everything under /logs/ not already covered by a dedicated panel (LLM logs, Schedule history) — run logs, chat sessions, budget/rate-limit state, etc." />
      <FileBrowserPanel v-if="section === 'workspace'" key="workspace" root="workspace" api-base="/workspace" title="Workspace"
        description="Raw browser over /workspace/ — uploads, scheduled task scratch files, and workspace/memory/ (short-term staging notes the hourly maintenance pass folds into memory/notes/)." />
      <EgressPanel v-if="section === 'egress'" />
      <LlmLogsPanel v-if="section === 'llm-logs'" />
      <AppsPanel v-if="section === 'apps'" />
    </div>
  </div>
</template>
