#!/usr/bin/env bash
# 核对远端 hub 跑的是不是本地这份代码。
#
# 起因：几次调试卡在"以为部署了新版、实际跑的是旧的"，然后开始怀疑代码。
# /health 现在带 git sha（不需要密钥），所以这件事可以一条命令答清楚。
#
# 用法：
#   scripts/check-deploy.sh                      # 用 ~/.kiboard/config 里的 KIBOARD_URL
#   scripts/check-deploy.sh http://host:26041    # 指定地址
set -uo pipefail

URL="${1:-}"
if [[ -z "${URL}" ]]; then
    URL=$(sed -n 's/^KIBOARD_URL=//p' "${HOME}/.kiboard/config" 2>/dev/null | head -1)
fi
if [[ -z "${URL}" ]]; then
    echo "不知道 hub 地址：传参数或在 ~/.kiboard/config 里写 KIBOARD_URL" >&2
    exit 2
fi

local_sha=$(git rev-parse --short=8 HEAD 2>/dev/null || echo unknown)
dirty=""
git diff --quiet 2>/dev/null || dirty=" (工作区有未提交改动)"

remote=$(curl -s -m 8 "${URL}/health") || { echo "连不上 ${URL}" >&2; exit 2; }
remote_sha=$(printf '%s' "${remote}" | sed -n 's/.*"sha":"\([^"]*\)".*/\1/p')
remote_ver=$(printf '%s' "${remote}" | sed -n 's/.*"version":"\([^"]*\)".*/\1/p')

echo "本地 HEAD : ${local_sha}${dirty}"
echo "远端 hub  : ${remote_sha:-无版本字段} ${remote_ver:+(${remote_ver})}"
echo "地址      : ${URL}"

if [[ -z "${remote_sha}" ]]; then
    echo
    echo "远端 /health 没有 sha 字段 —— 那是加版本号之前的旧版本，必须重新部署。"
    exit 1
fi
if [[ "${remote_sha}" == "${local_sha}" ]]; then
    echo
    echo "一致。远端跑的就是本地 HEAD。"
    exit 0
fi
echo
echo "不一致。远端不是本地 HEAD，重新部署后再测，否则会在旧代码上排查新问题。"
if git cat-file -e "${remote_sha}" 2>/dev/null; then
    echo "远端落后的提交："
    git log --oneline "${remote_sha}..HEAD" | sed 's/^/  /'
fi
exit 1
