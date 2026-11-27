{{- define "axle.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "axle.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "axle.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "axle.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "axle.selectorLabels" -}}
app.kubernetes.io/name: {{ include "axle.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "axle.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "axle.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* Name of the Secret holding the admin secret: the existing one, or the generated one. */}}
{{- define "axle.adminSecretName" -}}
{{- default (include "axle.fullname" .) .Values.ui.auth.secret.existingSecret }}
{{- end }}

{{/* AXL_UI_AUTH for ui.auth.mode. */}}
{{- define "axle.uiAuth" -}}
{{- $mode := .Values.ui.auth.mode -}}
{{- if eq $mode "trustedHeader" }}trusted-header{{ else }}{{ $mode }}{{ end -}}
{{- end }}

{{/* Fails the render on ui.auth combinations that cannot work. */}}
{{- define "axle.validateUi" -}}
{{- $auth := .Values.ui.auth -}}
{{- if eq $auth.mode "secret" -}}
{{- if and $auth.secret.value $auth.secret.existingSecret -}}
{{- fail "ui.auth.secret: set either value or existingSecret, not both" -}}
{{- end -}}
{{- if and $auth.secret.value (lt (len (trim $auth.secret.value)) 16) -}}
{{- fail "ui.auth.secret.value must be at least 16 characters" -}}
{{- end -}}
{{- end -}}
{{- if and (eq $auth.mode "trustedHeader") (not (trim $auth.trustedHeader.name)) -}}
{{- fail "ui.auth.mode=trustedHeader needs ui.auth.trustedHeader.name (the header your auth proxy sets)" -}}
{{- end -}}
{{- if and (ne $auth.mode "trustedHeader") (trim $auth.trustedHeader.name) -}}
{{- fail (printf "ui.auth.trustedHeader.name is set but ui.auth.mode is %s; set mode: trustedHeader to use it" $auth.mode) -}}
{{- end -}}
{{- end }}

{{/* AXL_ALLOWED_HOSTS: ingress hosts, the in-cluster service names and ui.allowedHosts. */}}
{{- define "axle.allowedHosts" -}}
{{- $svc := include "axle.fullname" . -}}
{{- $ns := .Release.Namespace -}}
{{- $hosts := list $svc (printf "%s.%s" $svc $ns) (printf "%s.%s.svc" $svc $ns) (printf "%s.%s.svc.cluster.local" $svc $ns) -}}
{{- if .Values.ingress.enabled -}}
{{- range .Values.ingress.hosts -}}
{{- $hosts = append $hosts .host -}}
{{- end -}}
{{- end -}}
{{- range .Values.ui.allowedHosts -}}
{{- $hosts = append $hosts . -}}
{{- end -}}
{{- $hosts | uniq | join "," -}}
{{- end }}

{{/* AXL_COOKIE_SECURE: explicit value, else on when the ingress terminates TLS. */}}
{{- define "axle.cookieSecure" -}}
{{- if kindIs "invalid" .Values.ui.cookieSecure -}}
{{- if and .Values.ingress.enabled .Values.ingress.tls }}true{{ else }}false{{ end -}}
{{- else -}}
{{- .Values.ui.cookieSecure -}}
{{- end -}}
{{- end }}

{{/* AXL_PUBLIC_URL: explicit publicUrl, else derived from the first ingress host, else empty. */}}
{{- define "axle.publicUrl" -}}
{{- if .Values.publicUrl -}}
{{- .Values.publicUrl -}}
{{- else if and .Values.ingress.enabled .Values.ingress.hosts -}}
{{- $host := (first .Values.ingress.hosts).host -}}
{{- if .Values.ingress.tls }}https{{ else }}http{{ end }}://{{ $host }}
{{- end -}}
{{- end }}
