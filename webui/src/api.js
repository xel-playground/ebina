// Thin fetch wrapper shared by every panel — token lives in localStorage,
// sent as a Bearer header (query param fallback for EventSource, which
// can't set headers at all).
const TOKEN_KEY = 'ebina_token'

export function token() {
  return localStorage.getItem(TOKEN_KEY) || ''
}

export function setToken(t) {
  localStorage.setItem(TOKEN_KEY, t)
}

export function clearToken() {
  localStorage.removeItem(TOKEN_KEY)
}

export async function api(path, opts = {}) {
  opts.headers = Object.assign({ Authorization: 'Bearer ' + token() }, opts.headers || {})
  const res = await fetch('/api' + path, opts)
  const text = await res.text()
  let body
  try {
    body = JSON.parse(text)
  } catch {
    body = text
  }
  return { status: res.status, body }
}

export function eventSource(path) {
  return new EventSource('/api' + path + '?token=' + encodeURIComponent(token()))
}
