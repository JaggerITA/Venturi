# Istruzioni per Claude Code

## Stile dei commenti

Regola ferrea: i commenti nel codice devono essere **brevi** e scritti
**solo quando necessari** — mai per spiegare cose ovvie o che si leggono
già dal codice stesso (nomi di variabili/funzioni ben scelti bastano).

Un commento è giustificato solo quando spiega un *perché* non ovvio: un
vincolo nascosto, il motivo di una scelta non scontata, un bug aggirato,
un comportamento che sorprenderebbe chi legge. Se togliendolo il codice
resta comunque chiaro, il commento non va scritto.

Evitare in particolare:
- Commenti-saggio di più paragrafi su una singola riga o funzione.
- Ripetere nel commento quello che il codice già dice (il *cosa* invece
  del *perché*).
- Premesse/contesto storico lunghi quando basterebbe una riga secca.
