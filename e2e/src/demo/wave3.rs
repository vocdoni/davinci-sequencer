//! The third wave of demo elections: a live meeting. Votes arrive a few at a
//! time through both nodes, which hold them for their batch time, and the
//! organizer closes each vote while they are still there: the grace window
//! lets every one settle before the results. In English, Spanish and Catalan.

use super::{Action, BallotKind, KeySource, Lang, Lifecycle, Refusal, Spec};

/// Wave-3 defaults: ballot modes as davinci-sdk builds them.
const W3: Spec = Spec {
    sdk: true,
    ..Spec::BASE
};

/// The third wave: three votes of one assembly.
pub fn elections() -> Vec<Spec> {
    vec![
        Spec {
            n: 1,
            dir: "wave3/1-board-chair",
            title: "Annual assembly: election of the board chair",
            description: "The annual general assembly elects the chair of the board for the next \
                two years. Four members stood as candidates. Vote from the hall or from home while \
                the item is open; the chair of the meeting closes the vote once the last ballot is \
                in, and the result is published shortly after.",
            question: "Who should chair the board?",
            question_description: "Pick one candidate.",
            choices: &["Marta Puig", "Joan Riera", "Laura Vidal", "Sergi Camps"],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Asamblea anual: elección de la presidencia de la junta",
                    description: "La asamblea general anual elige a quien presidirá la junta \
                        los próximos dos años. Se han presentado cuatro candidaturas. Vota desde \
                        la sala o desde casa mientras el punto esté abierto; la mesa cierra la \
                        votación cuando entra la última papeleta y el resultado se publica poco \
                        después.",
                    question: "¿Quién debería presidir la junta?",
                    question_description: "Elige una candidatura.",
                    choices: &["Marta Puig", "Joan Riera", "Laura Vidal", "Sergi Camps"],
                },
                Lang {
                    code: "ca",
                    title: "Assemblea anual: elecció de la presidència de la junta",
                    description: "L'assemblea general anual escull qui presidirà la junta els \
                        propers dos anys. S'hi han presentat quatre candidatures. Vota des de la \
                        sala o des de casa mentre el punt sigui obert; la mesa tanca la votació \
                        quan entra l'última papereta i el resultat es publica poc després.",
                    question: "Qui hauria de presidir la junta?",
                    question_description: "Tria una candidatura.",
                    choices: &["Marta Puig", "Joan Riera", "Laura Vidal", "Sergi Camps"],
                },
            ],
            lean: &[0.38, 0.27, 0.22, 0.13],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 16,
            key: KeySource::Node(0),
            lifecycle: Lifecycle::Meeting,
            round1: 16,
            round2: (16, 16),
            max_voters: 16,
            ..W3
        },
        Spec {
            n: 2,
            dir: "wave3/2-friday-opening-hours",
            title: "Motion: keep the social centre open until midnight on Fridays",
            description: "A member moved from the floor that the social centre stay open until \
                midnight on Fridays from next month, with volunteers taking turns at the door. \
                The chair opened a short vote and, once most of the hall had voted, announced \
                that voting closes in one minute.",
            question: "Do you support the motion?",
            question_description: "Pick one option.",
            choices: &["In favour", "Against", "Abstain"],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Moción: abrir el centro social hasta medianoche los viernes",
                    description: "Un socio ha propuesto desde la sala que el centro social abra \
                        hasta medianoche los viernes a partir del mes que viene, con turnos de \
                        voluntarios en la puerta. La mesa ha abierto una votación breve y, cuando \
                        la mayor parte de la sala ya había votado, ha anunciado que la votación se \
                        cierra en un minuto.",
                    question: "¿Apoyas la moción?",
                    question_description: "Elige una opción.",
                    choices: &["A favor", "En contra", "Abstención"],
                },
                Lang {
                    code: "ca",
                    title: "Moció: obrir el centre social fins a mitjanit els divendres",
                    description: "Un soci ha proposat des de la sala que el centre social obri \
                        fins a mitjanit els divendres a partir del mes vinent, amb torns de \
                        voluntaris a la porta. La mesa ha obert una votació breu i, quan la major \
                        part de la sala ja havia votat, ha anunciat que la votació es tanca d'aquí \
                        a un minut.",
                    question: "Dones suport a la moció?",
                    question_description: "Tria una opció.",
                    choices: &["A favor", "En contra", "Abstenció"],
                },
            ],
            lean: &[0.55, 0.3, 0.15],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 13,
            key: KeySource::Node(1),
            // Nine at the meeting's pace, three in the last minute; the
            // thirteenth member votes too late.
            lifecycle: Lifecycle::MeetingCloses { after: 9 },
            actions: &[Action::Refuse(Refusal::AfterEnd)],
            round1: 12,
            round2: (12, 12),
            max_voters: 13,
            ..W3
        },
        Spec {
            n: 3,
            dir: "wave3/3-budget-2027",
            title: "Approval of the 2027 budget",
            description: "The board presents next year's budget to the assembly: 48,300 euros of \
                income and 47,900 euros of spending, with the difference going to the reserve \
                fund. The chair closes the vote once the last ballot is in. The key that opens \
                the ballots is held by an independent committee, which decrypts only the final \
                count.",
            question: "Do you approve the 2027 budget?",
            question_description: "Pick one option.",
            choices: &["Approve", "Reject", "Abstain"],
            i18n: &[
                Lang {
                    code: "es",
                    title: "Aprobación del presupuesto de 2027",
                    description: "La junta presenta a la asamblea el presupuesto del año que \
                        viene: 48.300 euros de ingresos y 47.900 euros de gastos, y la diferencia \
                        va al fondo de reserva. La mesa cierra la votación cuando entra la última \
                        papeleta. La clave que abre las papeletas la custodia un comité \
                        independiente, que solo descifra el recuento final.",
                    question: "¿Apruebas el presupuesto de 2027?",
                    question_description: "Elige una opción.",
                    choices: &["Apruebo", "Rechazo", "Abstención"],
                },
                Lang {
                    code: "ca",
                    title: "Aprovació del pressupost del 2027",
                    description: "La junta presenta a l'assemblea el pressupost de l'any vinent: \
                        48.300 euros d'ingressos i 47.900 euros de despeses, i la diferència va \
                        al fons de reserva. La mesa tanca la votació quan entra l'última \
                        papereta. La clau que obre les paperetes la custodia un comitè \
                        independent, que només desxifra el recompte final.",
                    question: "Aproves el pressupost del 2027?",
                    question_description: "Tria una opció.",
                    choices: &["Aprovo", "Rebutjo", "Abstenció"],
                },
            ],
            lean: &[0.7, 0.18, 0.12],
            ballot: BallotKind::SingleChoice { abstain: false },
            members: 12,
            key: KeySource::DkgAutomatic,
            lifecycle: Lifecycle::Meeting,
            round1: 12,
            round2: (12, 12),
            max_voters: 12,
            ..W3
        },
    ]
}
