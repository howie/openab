#!/usr/bin/env bash
# Helm template tests for openab chart — bot messages config
set -euo pipefail

CHART_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PASS=0
FAIL=0

pass() { ((PASS++)); echo "  PASS: $1"; }
fail() { ((FAIL++)); echo "  FAIL: $1"; }

echo "=== Helm template tests: allowBotMessages & trustedBotIds ==="
echo

# ---------- Test 1: allowBotMessages = "mentions" renders correctly ----------
echo "[Test 1] allowBotMessages = mentions renders correctly"
OUT=$(helm template test "$CHART_DIR" \
  --set 'agents.kiro.discord.allowBotMessages=mentions' 2>&1)
if echo "$OUT" | grep -q 'allow_bot_messages = "mentions"'; then
  pass "allow_bot_messages = \"mentions\" found in rendered output"
else
  fail "allow_bot_messages = \"mentions\" not found in rendered output"
  echo "$OUT"
fi

# ---------- Test 2: allowBotMessages = "all" renders correctly ----------
echo "[Test 2] allowBotMessages = all renders correctly"
OUT=$(helm template test "$CHART_DIR" \
  --set 'agents.kiro.discord.allowBotMessages=all' 2>&1)
if echo "$OUT" | grep -q 'allow_bot_messages = "all"'; then
  pass "allow_bot_messages = \"all\" found in rendered output"
else
  fail "allow_bot_messages = \"all\" not found in rendered output"
  echo "$OUT"
fi

# ---------- Test 3: allowBotMessages = "off" renders correctly ----------
echo "[Test 3] allowBotMessages = off renders correctly"
OUT=$(helm template test "$CHART_DIR" \
  --set 'agents.kiro.discord.allowBotMessages=off' 2>&1)
if echo "$OUT" | grep -q 'allow_bot_messages = "off"'; then
  pass "allow_bot_messages = \"off\" found in rendered output"
else
  fail "allow_bot_messages = \"off\" not found in rendered output"
  echo "$OUT"
fi

# ---------- Test 4: invalid allowBotMessages value fails ----------
echo "[Test 4] invalid allowBotMessages value is rejected"
OUT=$(helm template test "$CHART_DIR" \
  --set 'agents.kiro.discord.allowBotMessages=yolo' 2>&1) && RC=0 || RC=$?
if [ "$RC" -ne 0 ] && echo "$OUT" | grep -q 'must be one of: off, mentions, all'; then
  pass "invalid value 'yolo' rejected with correct error message"
else
  fail "invalid value 'yolo' was not rejected or error message is wrong"
  echo "$OUT"
fi

# ---------- Test 5: trustedBotIds renders correctly ----------
echo "[Test 5] trustedBotIds renders correctly"
OUT=$(helm template test "$CHART_DIR" \
  --set-string 'agents.kiro.discord.trustedBotIds[0]=123456789012345678' \
  --set-string 'agents.kiro.discord.trustedBotIds[1]=987654321098765432' \
  --set 'agents.kiro.discord.allowBotMessages=mentions' 2>&1)
if echo "$OUT" | grep -q 'trusted_bot_ids = \["123456789012345678","987654321098765432"\]'; then
  pass "trustedBotIds rendered as JSON array"
else
  fail "trustedBotIds not rendered correctly"
  echo "$OUT"
fi

# ---------- Test 6: mangled trustedBotId (--set not --set-string) fails ----------
echo "[Test 6] mangled snowflake ID via --set is rejected"
OUT=$(helm template test "$CHART_DIR" \
  --set 'agents.kiro.discord.trustedBotIds[0]=1.234567890123457e+17' 2>&1) && RC=0 || RC=$?
if [ "$RC" -ne 0 ] && echo "$OUT" | grep -q 'mangled ID'; then
  pass "mangled snowflake ID rejected with correct error"
else
  fail "mangled snowflake ID was not rejected"
  echo "$OUT"
fi

# ---------- Test 7: default allowBotMessages="off" does not omit the field ----------
echo "[Test 7] default values render allow_bot_messages"
OUT=$(helm template test "$CHART_DIR" 2>&1)
if echo "$OUT" | grep -q 'allow_bot_messages = "off"'; then
  pass "default allow_bot_messages = \"off\" rendered"
else
  fail "default allow_bot_messages = \"off\" not found in rendered output"
  echo "$OUT"
fi

echo
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ] || exit 1
