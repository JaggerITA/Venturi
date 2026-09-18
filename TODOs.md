FINESTRA DI PREVIEW:
[x] Quando Premo CTRL+f, il player deve andare a schermo intero. nella vista a schermo intero, compare un overlay in basso (solo al passaggio col mouse) con la barra di riproduzione (interagibile) e il tasto play/pausa - si esce con ESC.

SHORTCUTS:
[x] Attualmente sono fissate. Devono essere configurabili dall'utente:
- Aggiungi un menu File -> Impostazioni che apre un dialog diviso in sezioni. la prima è "Scorciatoie da tastiera". Elenca tutte quelle presenti e permette all'utente di modificarle.
- Salviamo le impostazioni utente (relative al programma) in ~/.config/vibevideo

PANNELLO inspector (quello sulla destra, con le proprietà e i parametri, keyframe, ecc):
[ ] Leggera modifica alla logica di applicazione degli attributi su clip multiple: attualmente, selezionando più clip, vengono applicati i parametri impostati nel pannello a tutte le clip. La granularità però deve essere sul singolo parametro toccato. ES: se muovo soltanto "posizione Y", tutte le clip selezionate devono mantenere gli attuali valori e solo "posizione Y" deve essere impostata su tutte le clip selezionate. NON anche posizione X e/o altre.
