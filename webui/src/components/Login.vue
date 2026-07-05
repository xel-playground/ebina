<script setup>
import { ref } from 'vue'
import { api, setToken } from '../api'

const emit = defineEmits(['logged-in'])
const loginToken = ref('')
const loginError = ref('')

async function login() {
  setToken(loginToken.value)
  const { status } = await api('/status')
  if (status === 200) {
    loginError.value = ''
    emit('logged-in')
  } else {
    loginError.value = 'wrong token'
  }
}
</script>

<template>
  <div class="login-shell">
    <div class="card login-card">
      <div class="brand"><span class="dot" style="background:#9ca3af"></span> ebina gateway</div>
      <p class="hint">Token lives in <code>secrets.toml</code> as <code>gateway_token</code>.</p>
      <input type="password" v-model="loginToken" placeholder="gateway_token" @keyup.enter="login">
      <div class="row"><button @click="login">Log in</button></div>
      <p class="error" v-if="loginError">{{ loginError }}</p>
    </div>
  </div>
</template>
