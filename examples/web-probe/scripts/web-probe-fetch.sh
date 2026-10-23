#!/bin/sh
# web-probe fetch step: one business's home page, plus its pricing, services,
# contact and about pages when the home page links to them on the same site,
# printed as tagged plain text for the web-probe role. Fetching happens here,
# on this machine; the model never touches the network.
#
# Usage: web-probe-fetch.sh <url>   (a bare example.com is read as https)
set -eu

url=${1:?usage: web-probe-fetch.sh <url>}
case "$url" in http://*|https://*) ;; *) url="https://$url" ;; esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# fetch <url> <file>: body to <file>, final URL (after redirects) on stdout.
fetch() {
  curl -q --globoff --silent --show-error --fail --location --max-redirs 5 \
    --proto =http,https --proto-redir =http,https \
    --max-time 20 --max-filesize 5000000 --compressed \
    --user-agent "Mozilla/5.0 (compatible; darkmux-web-probe)" \
    --header "Accept: text/html,application/xhtml+xml;q=0.9,text/plain;q=0.8" \
    --output "$2" --write-out '%{url_effective}' -- "$1"
}

# to_text <file>: visible text. textutil (macOS) when present, else a tag strip.
to_text() {
  if command -v textutil >/dev/null 2>&1; then
    textutil -convert txt -format html -inputencoding UTF-8 -stdin -stdout < "$1" 2>/dev/null
  else
    perl -0777 -pe 's/<(script|style|noscript|svg|template|title)\b.*?<\/\1\s*>//gis; s/<!--.*?-->//gs; s/<br\b[^>]*>|<\/(p|div|li|h[1-6]|tr|section|header|footer|address)>/\n/gi; s/<[^>]+>/ /g; s/&nbsp;/ /g; s/&amp;/&/g; s/&lt;/</g; s/&gt;/>/g; s/&quot;/"/g; s/&#39;/'"'"'/g' "$1"
  fi | tr -s ' \t' ' ' | sed 's/^ //; s/ $//' | grep -v '^$' || true
}

# page <file> <final-url> <kind> <max-chars>: one tagged page block.
page() {
  printf '<web-page url="%s" kind="%s">\n' "$2" "$3"
  perl -0777 -ne '
    my $t = /<title[^>]*>(.*?)<\/title>/is ? $1 : ""; $t =~ s/\s+/ /g;
    my $d = /<meta[^>]+(?:name|property)=["\x27](?:og:)?description["\x27][^>]*content=["\x27]([^"\x27]*)/is ? $1 : "";
    print "<title>$t</title>\n<meta-description>$d</meta-description>\n<structured-data>\n";
    my $sd = ""; $sd .= "$1\n" while /<script[^>]*ld\+json[^>]*>(.*?)<\/script>/gis;
    print substr($sd, 0, 6000), "</structured-data>\n<contact-links>\n";
    my %seen; while (/href=["\x27]((?:tel|mailto):[^"\x27]+)/gi) { print "$1\n" unless $seen{lc $1}++ }
    print "</contact-links>\n";
  ' "$1"
  printf '<page-text>\n'
  to_text "$1" | head -c "$4"
  printf '\n</page-text>\n</web-page>\n'
}

home_url=$(fetch "$url" "$tmp/home.html") || { echo "could not fetch $url" >&2; exit 1; }
page "$tmp/home.html" "$home_url" home 20000

# Same-site links worth reading: the first one found for each kind.
origin=$(printf '%s' "$home_url" | sed -E 's#^(https?://[^/]+).*#\1#')
ORIGIN="$origin" HOME_URL="$home_url" perl -0777 -ne '
  my ($origin, $home) = ($ENV{ORIGIN}, $ENV{HOME_URL});
  my %kinds = (pricing => qr/pric|cost|rates|fees|quote/i, services => qr/servic/i,
               contact => qr/contact/i, about => qr/about/i);
  my %got;
  while (/<a\b([^>]*)>(.*?)<\/a>/gis) {
    my ($attrs, $label) = ($1, $2);
    next unless $attrs =~ /href=["\x27]([^"\x27#]+)/i;
    my $h = $1;
    next if $h =~ /^(mailto|tel|javascript|data):/i;
    if ($h =~ m{^//}) { next }
    elsif ($h =~ m{^https?://}i) { next unless $h =~ m{^\Q$origin\E(/|$)}i }
    elsif ($h =~ m{^/}) { $h = $origin . $h }
    else { $h = "$origin/$h" }
    next if $h eq $home || "$h/" eq $home;
    for my $k (sort keys %kinds) {
      next if $got{$k};
      if ($h =~ $kinds{$k} || $label =~ $kinds{$k}) { $got{$k} = $h; last }
    }
  }
  print "$_ $got{$_}\n" for grep { $got{$_} } qw(pricing services contact about);
' "$tmp/home.html" > "$tmp/links" 2>/dev/null || true

while read -r kind link; do
  [ -n "$link" ] || continue
  final=$(fetch "$link" "$tmp/$kind.html" 2>/dev/null) || { printf '<web-page url="%s" kind="%s" fetched="no"></web-page>\n' "$link" "$kind"; continue; }
  # A link that redirected off the site is not this business's page.
  case "$final" in "$origin" | "$origin"/*) page "$tmp/$kind.html" "$final" "$kind" 8000 ;; esac
done < "$tmp/links"
