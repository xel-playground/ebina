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
import SkillsPanel from './components/SkillsPanel.vue'
import GrantsPanel from './components/GrantsPanel.vue'
import ReportsPanel from './components/ReportsPanel.vue'
import ConfigPanel from './components/ConfigPanel.vue'
import SecretsPanel from './components/SecretsPanel.vue'
import LogsPanel from './components/LogsPanel.vue'
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
      <ChatPanel v-if="section === 'chat'" />
      <StatusPanel v-if="section === 'status'" />
      <SchedulerPanel v-if="section === 'scheduler'" />
      <ScheduleHistoryPanel v-if="section === 'schedule-history'" />
      <NotesPanel v-if="section === 'notes'" />
      <SoulPanel v-if="section === 'soul'" />
      <SkillsPanel v-if="section === 'skills'" />
      <GrantsPanel v-if="section === 'grants'" />
      <ReportsPanel v-if="section === 'report'" />
      <ConfigPanel v-if="section === 'config'" />
      <SecretsPanel v-if="section === 'secrets'" />
      <LogsPanel v-if="section === 'logs'" />
      <EgressPanel v-if="section === 'egress'" />
      <LlmLogsPanel v-if="section === 'llm-logs'" />
      <AppsPanel v-if="section === 'apps'" />
    </div>
  </div>
</template>
