{{- define "lodestore.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "lodestore.labels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end -}}

{{- define "lodestore.selectorLabels" -}}
app.kubernetes.io/name: {{ .Chart.Name }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "lodestore.secretName" -}}
{{- default (include "lodestore.fullname" .) .Values.existingSecret -}}
{{- end -}}

{{- define "lodestore.databaseSecretName" -}}
{{- default (include "lodestore.secretName" .) .Values.externalDatabase.existingSecret -}}
{{- end -}}

{{- define "lodestore.hasDatabase" -}}
{{- if or .Values.externalDatabase.url .Values.externalDatabase.existingSecret -}}true{{- end -}}
{{- end -}}

{{- define "lodestore.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "lodestore.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}
