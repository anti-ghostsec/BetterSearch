#!/bin/sh
# probe-site: tells you whether a booru can be added in BetterSearch, and
# which API to pick. Reads no config, changes nothing.
#
#   tools/probe-site.sh booru.example
#
# Prints the API family that works, or why none does. Only https matters, and
# only the shapes the app already speaks are tried. Exit 0 means addable.

set -u
UA='Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0'
host=${1:-}
[ -n "$host" ] || { echo "usage: $0 <booru host>" >&2; exit 2; }
host=${host#https://}
host=${host%%/*}
[ -n "$host" ] || { echo "no host in that" >&2; exit 2; }

tmp=$(mktemp) || exit 1
trap 'rm -f "$tmp"' EXIT

# $1 = label, $2 = URL. Prints the body's first bytes on a hit.
try() {
	code=$(curl -s -o "$tmp" -w '%{http_code}' --max-time 10 -A "$UA" "$2" 2>/dev/null)
	type=$(head -c 200 "$tmp" 2>/dev/null | tr -d '\n')
	case "$code:$type" in
		200:\[*|200:\{*) return 0 ;;
		*) return 1 ;;
	esac
}

echo "probing $host"
echo

if try gelbooru "https://$host/index.php?page=dapi&s=post&q=index&json=1&limit=1"; then
	echo "ADDABLE: API family \"Gelbooru 0.2\""
	img_api="https://$host/index.php?page=dapi&s=post&q=index&json=1&limit=1"
elif try danbooru "https://$host/posts.json?limit=1"; then
	echo "ADDABLE: API family \"Danbooru 2\""
	img_api="https://$host/posts.json?limit=1"
elif try moebooru "https://$host/post.json?limit=1"; then
	echo "ADDABLE: API family \"Moebooru\""
	img_api="https://$host/post.json?limit=1"
elif try e621 "https://$host/posts.json?search[limit]=1" && grep -q '"posts"' "$tmp"; then
	echo "ADDABLE: API family \"e621\""
	img_api="https://$host/posts.json?search[limit]=1"
else
	code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -A "$UA" "https://$host/" 2>/dev/null)
	case "$code" in
		000) echo "no answer over https (dead, IPv6-only, or blocking this network)" ;;
		403|503) echo "reachable but blocked: an interactive challenge or a WAF. Not addable without a cookie." ;;
		302|301) echo "redirects; try the address it lands on" ;;
		404) echo "answers, but not at that address" ;;
		*) echo "not addable: no Gelbooru, Danbooru, Moebooru or e621 API answered (HTTP $code)" ;;
	esac
	# Shimmie answers HTML at /api/danbooru/posts; a forum has no API at all.
	case "$(head -c 400 "$tmp" 2>/dev/null)" in
		*'id="XF"'*) echo "It's a XenForo forum — a community, not a booru." ;;
	esac
	exit 1
fi

# The image hosts of live boorus sometimes check the referer. Report it, so a
# site that searches but can't show pictures isn't a surprise later.
echo
if try image "$img_api"; then
	url=$(grep -oE '"(preview_url|file_url)":"[^"]+"' "$tmp" | head -1 | cut -d'"' -f4 | sed 's#\\/#/#g')
	url=${url:-$(grep -oE '"sample_url":"[^"]+"' "$tmp" | head -1 | cut -d'"' -f4 | sed 's#\\/#/#g')}
	if [ -n "$url" ]; then
		plain=$(curl -s -o "$tmp" -w '%{content_type}' --max-time 10 "$url" 2>/dev/null)
		ref=$(curl -s -o /dev/null -w '%{content_type}' --max-time 10 -H "Referer: https://$host/" "$url" 2>/dev/null)
		case "$plain" in
			image/*) echo "images: fine without a referer" ;;
			*) case "$ref" in
				image/*) echo "images: need the site as referer (the app already does this for Gelbooru-family sites)" ;;
				*) echo "images: $plain without, $ref with a referer — check by eye" ;;
			esac ;;
		esac
	fi
fi
echo
echo "Add it in Settings -> Custom boorus, with the family above."
exit 0
