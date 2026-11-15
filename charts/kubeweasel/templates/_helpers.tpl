{{- define "kubeweasel.privateProxyImage" -}}
{{- $private := .Values.privateHttp | default dict -}}
{{- default "ghcr.io/nginx/nginx-unprivileged@sha256:6b0610b60dff09248d642643a29393543a92eb355b00bbd1d12c7af562ef94c4" $private.proxyImage -}}
{{- end -}}
