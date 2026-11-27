{{- define "coord.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "coord.fullname" -}}
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

{{- define "coord.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "coord.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "coord.selectorLabels" -}}
app.kubernetes.io/name: {{ include "coord.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "coord.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "coord.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* Name of the Secret holding the admin secret: the existing one, or the generated one. */}}
{{- define "coord.adminSecretName" -}}
{{- default (include "coord.fullname" .) .Values.adminSecret.existingSecret }}
{{- end }}

{{/* COORD_COOKIE_SECURE: explicit value, else on when the ingress terminates TLS. */}}
{{- define "coord.cookieSecure" -}}
{{- if kindIs "invalid" .Values.cookieSecure -}}
{{- if and .Values.ingress.enabled .Values.ingress.tls }}true{{ else }}false{{ end -}}
{{- else -}}
{{- .Values.cookieSecure -}}
{{- end -}}
{{- end }}
